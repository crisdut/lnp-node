// LNP Node: node running lightning network protocol and generalized lightning
// channels.
// Written in 2020-2022 by
//     Dr. Maxim Orlovsky <orlovsky@lnp-bp.org>
//
// To the extent possible under law, the author(s) have dedicated all
// copyright and related and neighboring rights to this software to
// the public domain worldwide. This software is distributed without
// any warranty.
//
// You should have received a copy of the MIT License along with this software.
// If not, see <https://opensource.org/licenses/MIT>.

use std::fs;

use amplify::Wrapper;
use bitcoin::secp256k1::{self, Secp256k1};
use bitcoin::util::bip32::{ChildNumber, DerivationPath, ExtendedPrivKey};
use bitcoin::XpubIdentifier;
use lnp::channel::bolt::LocalKeyset;
use lnp::p2p::bolt::ChannelId;
use lnpbp::chain::Chain;
use microservices::esb::{self, Handler};
use strict_encoding::StrictDecode;
use wallet::psbt::sign::{MemoryKeyProvider, MemorySigningAccount, SecretProvider, SignAll};

use crate::bus::{BusMsg, CtlMsg, ServiceBus};
use crate::rpc::ServiceId;
use crate::{Config, Endpoints, Error, Service, LNP_NODE_MASTER_KEY_FILE};

pub fn run(config: Config) -> Result<(), Error> {
    let secp = Secp256k1::new();
    let runtime = Runtime::with(&secp, &config)?;
    Service::run(config, runtime, false)
}

pub struct Runtime<'secp>
where
    Self: 'secp,
{
    chain: Chain,
    provider: MemoryKeyProvider<'secp, secp256k1::All>,
}

impl<'secp> Runtime<'secp>
where
    Self: 'secp,
{
    pub fn with(secp: &'secp Secp256k1<secp256k1::All>, config: &Config) -> Result<Self, Error> {
        Ok(Runtime { chain: config.chain.clone(), provider: Runtime::provider(secp, config)? })
    }

    fn provider(
        secp: &'secp Secp256k1<secp256k1::All>,
        config: &Config,
    ) -> Result<MemoryKeyProvider<'secp, secp256k1::All>, Error> {
        let mut wallet_path = config.data_dir.clone();
        wallet_path.push(LNP_NODE_MASTER_KEY_FILE);

        let mut file = fs::File::open(wallet_path)?;
        let master_id = XpubIdentifier::strict_decode(&mut file)?;
        let derivation = DerivationPath::strict_decode(&mut file)?;
        let account_xpriv = ExtendedPrivKey::strict_decode(&mut file)?;
        let signing_account =
            MemorySigningAccount::with(secp, master_id, derivation, account_xpriv);
        let mut provider = MemoryKeyProvider::with(secp, true);
        provider.add_account(signing_account);
        Ok(provider)
    }
}

impl<'secp> esb::Handler<ServiceBus> for Runtime<'secp>
where
    Self: 'secp,
{
    type Request = BusMsg;
    type Error = Error;

    fn identity(&self) -> ServiceId { ServiceId::Signer }

    fn handle(
        &mut self,
        endpoints: &mut Endpoints,
        bus: ServiceBus,
        source: ServiceId,
        message: BusMsg,
    ) -> Result<(), Self::Error> {
        match (bus, message, source) {
            (ServiceBus::Ctl, BusMsg::Ctl(msg), source) => {
                if let Err(err) = self.handle_ctl(endpoints, source.clone(), msg.clone()) {
                    endpoints.send_to(
                        ServiceBus::Ctl,
                        self.identity(),
                        source.clone(),
                        BusMsg::Ctl(CtlMsg::with_error(&source, &msg, &err)),
                    )?;
                    Err(err)
                } else {
                    Ok(())
                }
            }
            (bus, msg, _) => Err(Error::wrong_esb_msg(bus, &msg)),
        }
    }

    fn handle_err(
        &mut self,
        _: &mut Endpoints,
        _: esb::Error<ServiceId>,
    ) -> Result<(), Self::Error> {
        // We do nothing and do not propagate error; it's already being reported
        // with `error!` macro by the controller. If we propagate error here
        // this will make whole daemon panic
        Ok(())
    }
}

impl<'secp> Runtime<'secp>
where
    Self: 'secp,
{
    fn handle_ctl(
        &mut self,
        endpoints: &mut Endpoints,
        source: ServiceId,
        message: CtlMsg,
    ) -> Result<(), Error> {
        match message {
            CtlMsg::Sign(mut psbt) => {
                let sig_count = psbt.sign_all(&self.provider)?;
                let txid = psbt.to_txid();
                info!("Transaction {} is signed ({} signatures added)", txid, sig_count);
                trace!("Signed PSBT: {:#?}", psbt);
                endpoints.send_to(
                    ServiceBus::Ctl,
                    ServiceId::Signer,
                    source,
                    BusMsg::Ctl(CtlMsg::Signed(psbt)),
                )?;
            }

            CtlMsg::DeriveKeyset(slice32) => {
                let mut buf = [0u8; 4];
                buf.copy_from_slice(&slice32.as_inner()[..4]);
                let le = u32::from_be_bytes(buf);
                let channel_index = le & 0x7FFFFFFF;
                if let Some(account) = self.provider.into_iter().next() {
                    let account_xpriv = account.account_xpriv();
                    let chain_index = self.chain.chain_params().is_testnet as u32;
                    let path = &[chain_index, 1, 0, channel_index]
                        .iter()
                        .map(|idx| ChildNumber::from_hardened_idx(*idx).expect("hardcoded index"))
                        .collect::<Vec<_>>();
                    let channel_xpriv =
                        account_xpriv.derive_priv(self.provider.secp_context(), path)?;
                    let keyset = LocalKeyset::with(
                        self.provider.secp_context(),
                        (account.account_fingerprint(), DerivationPath::from(path.as_ref())),
                        channel_xpriv,
                        // TODO: Use a key from a funding wallet
                        None,
                    );

                    endpoints.send_to(
                        ServiceBus::Ctl,
                        self.identity(),
                        source,
                        BusMsg::Ctl(CtlMsg::Keyset(
                            ServiceId::Channel(ChannelId::from_inner(slice32)),
                            keyset,
                        )),
                    )?;
                }
            }

            wrong_msg => {
                error!("Request {} is not supported by the CTL interface", wrong_msg);
                return Err(Error::wrong_esb_msg(ServiceBus::Ctl, &wrong_msg));
            }
        }

        Ok(())
    }
}
