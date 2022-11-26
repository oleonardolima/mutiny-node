use crate::event::{EventHandler, HTLCStatus, MillisatAmount, PaymentInfo};
use crate::invoice::create_phantom_invoice;
use crate::ldkstorage::{MutinyNodePersister, PhantomChannelManager};
use crate::localstorage::MutinyBrowserStorage;
use crate::nodemanager::MutinyInvoice;
use crate::utils::{currency_from_network, sleep};
use crate::wallet::MutinyWallet;
use bitcoin::hashes::sha256::Hash as Sha256;
use bitcoin::hashes::Hash;
use bitcoin::Network;
use futures::StreamExt;
use gloo_net::websocket::Message;
use lightning::chain::{chainmonitor, Filter, Watch};
use lightning::ln::channelmanager::PhantomRouteHints;
use lightning::ln::msgs::NetAddress;
use lightning::ln::{PaymentHash, PaymentPreimage};
use lightning::util::logger::{Logger, Record};
use std::collections::HashMap;
use std::net::{SocketAddr, ToSocketAddrs};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use wasm_bindgen_futures::spawn_local;

use crate::chain::MutinyChain;
use crate::error::MutinyStorageError;
use crate::tcpproxy::{SocketDescriptor, TcpProxy};
use crate::{
    background::{BackgroundProcessor, GossipSync},
    error::MutinyError,
    keymanager::{create_keys_manager, pubkey_from_keys_manager},
    logging::MutinyLogger,
    nodemanager::NodeIndex,
};
use anyhow::Context;
use bip39::Mnemonic;
use bitcoin::blockdata::constants::genesis_block;
use bitcoin::secp256k1::PublicKey;
use bitcoin_hashes::hex::ToHex;
use instant::SystemTime;
use lightning::chain::keysinterface::{
    InMemorySigner, KeysInterface, PhantomKeysManager, Recipient,
};
use lightning::ln::peer_handler::{
    IgnoringMessageHandler, MessageHandler as LdkMessageHandler, PeerManager as LdkPeerManager,
    SocketDescriptor as LdkSocketDescriptor,
};
use lightning::routing::gossip;
use lightning::routing::scoring::{ProbabilisticScorer, ProbabilisticScoringParameters};
use lightning::util::config::{ChannelHandshakeConfig, ChannelHandshakeLimits, UserConfig};
use lightning_invoice::utils::DefaultRouter;
use lightning_invoice::{payment, Invoice};
use log::{debug, error, info, trace, warn};

pub(crate) type NetworkGraph = gossip::NetworkGraph<Arc<MutinyLogger>>;

pub(crate) type MessageHandler = LdkMessageHandler<
    Arc<PhantomChannelManager>,
    Arc<IgnoringMessageHandler>,
    Arc<IgnoringMessageHandler>,
>;

pub(crate) type PeerManager = LdkPeerManager<
    SocketDescriptor,
    Arc<PhantomChannelManager>,
    Arc<IgnoringMessageHandler>,
    Arc<IgnoringMessageHandler>,
    Arc<MutinyLogger>,
    Arc<IgnoringMessageHandler>,
>;

pub(crate) type ChainMonitor = chainmonitor::ChainMonitor<
    InMemorySigner,
    Arc<dyn Filter + Send + Sync>,
    Arc<MutinyChain>,
    Arc<MutinyChain>,
    Arc<MutinyLogger>,
    Arc<MutinyNodePersister>,
>;

pub(crate) type InvoicePayer<E> =
    payment::InvoicePayer<Arc<PhantomChannelManager>, Router, Arc<MutinyLogger>, E>;

type Router = DefaultRouter<
    Arc<NetworkGraph>,
    Arc<MutinyLogger>,
    Arc<Mutex<ProbabilisticScorer<Arc<NetworkGraph>, Arc<MutinyLogger>>>>,
>;

pub struct Node {
    pub uuid: String,
    pub pubkey: PublicKey,
    pub peer_manager: Arc<PeerManager>,
    pub keys_manager: Arc<PhantomKeysManager>,
    pub chain: Arc<MutinyChain>,
    pub channel_manager: Arc<PhantomChannelManager>,
    pub chain_monitor: Arc<ChainMonitor>,
    pub invoice_payer: Arc<InvoicePayer<EventHandler>>,
    network: Network,
    pub persister: Arc<MutinyNodePersister>,
    _background_processor: BackgroundProcessor,
    logger: Arc<MutinyLogger>,
    websocket_proxy_addr: String,
    temp_peer_connection_map: Arc<Mutex<HashMap<String, String>>>,
}

impl Node {
    pub(crate) async fn new(
        node_index: NodeIndex,
        mnemonic: Mnemonic,
        storage: MutinyBrowserStorage,
        chain: Arc<MutinyChain>,
        wallet: Arc<MutinyWallet>,
        network: Network,
        websocket_proxy_addr: String,
    ) -> Result<Self, MutinyError> {
        info!("initialized a new node: {}", node_index.uuid);

        let logger = Arc::new(MutinyLogger::default());

        let keys_manager = Arc::new(create_keys_manager(mnemonic, node_index.child_index));
        let pubkey = pubkey_from_keys_manager(&keys_manager);

        // init the persister
        let persister = Arc::new(MutinyNodePersister::new(node_index.uuid.clone(), storage));

        // init chain monitor
        let chain_monitor: Arc<ChainMonitor> = Arc::new(ChainMonitor::new(
            None,
            chain.clone(),
            logger.clone(),
            chain.clone(),
            persister.clone(),
        ));

        // read channelmonitor state from disk
        let channel_monitors = persister
            .read_channel_monitors(keys_manager.clone())
            .map_err(|e| MutinyError::ReadError {
                source: MutinyStorageError::Other(e.into()),
            })?;

        // init channel manager
        let (channel_manager, restarting_node) = persister
            .read_channel_manager(
                network,
                chain_monitor.clone(),
                chain.clone(),
                logger.clone(),
                keys_manager.clone(),
                channel_monitors,
            )
            .await?;
        let channel_manager: Arc<PhantomChannelManager> = Arc::new(channel_manager);

        // init peer manager
        let ln_msg_handler = MessageHandler {
            chan_handler: channel_manager.clone(),
            route_handler: Arc::new(IgnoringMessageHandler {}),
            onion_message_handler: Arc::new(IgnoringMessageHandler {}),
        };

        // init event handler
        let event_handler = EventHandler::new(
            channel_manager.clone(),
            chain.clone(),
            wallet.clone(),
            keys_manager.clone(),
            persister.clone(),
            network,
            logger.clone(),
        );
        let peer_man = Arc::new(create_peer_manager(
            keys_manager.clone(),
            ln_msg_handler,
            logger.clone(),
        ));

        // fixme dont read from storage twice lol
        // read channelmonitor state from disk again
        let mut channel_monitors = persister
            .read_channel_monitors(keys_manager.clone())
            .map_err(|e| MutinyError::ReadError {
                source: MutinyStorageError::Other(e.into()),
            })?;

        // sync to chain tip
        let mut chain_listener_channel_monitors = Vec::new();
        if restarting_node {
            for (blockhash, channel_monitor) in channel_monitors.drain(..) {
                let outpoint = channel_monitor.get_funding_txo().0;
                chain_listener_channel_monitors.push((
                    blockhash,
                    (
                        channel_monitor,
                        chain.clone(),
                        chain.clone(),
                        logger.clone(),
                    ),
                    outpoint,
                ));
            }
        }

        // give channel monitors to chain monitor
        for item in chain_listener_channel_monitors.drain(..) {
            let channel_monitor = item.1 .0;
            let funding_outpoint = item.2;
            chain_monitor
                .clone()
                .watch_channel(funding_outpoint, channel_monitor);
        }

        // todo use RGS
        // get network graph
        let genesis_hash = genesis_block(network).block_hash();
        let network_graph = Arc::new(persister.read_network_graph(genesis_hash, logger.clone()));

        // create scorer
        let params = ProbabilisticScoringParameters::default();
        let scorer = Arc::new(Mutex::new(ProbabilisticScorer::new(
            params,
            network_graph.clone(),
            logger.clone(),
        )));

        let router: Router = DefaultRouter::new(
            network_graph,
            logger.clone(),
            keys_manager.get_secure_random_bytes(),
            scorer.clone(),
        );

        let invoice_payer: Arc<InvoicePayer<EventHandler>> = Arc::new(InvoicePayer::new(
            Arc::clone(&channel_manager),
            router,
            Arc::clone(&logger),
            event_handler,
            payment::Retry::Attempts(5), // todo potentially rethink
        ));

        let background_processor_logger = logger.clone();
        let background_processor_invoice_payer = invoice_payer.clone();
        let background_processor_peer_manager = peer_man.clone();
        let background_processor_channel_manager = channel_manager.clone();
        let background_chain_monitor = chain_monitor.clone();
        let gs: GossipSync<_, _, &NetworkGraph, _, Arc<MutinyLogger>> = GossipSync::none();

        let background_processor = BackgroundProcessor::start(
            persister.clone(),
            background_processor_invoice_payer.clone(),
            background_chain_monitor.clone(),
            background_processor_channel_manager.clone(),
            gs,
            background_processor_peer_manager.clone(),
            background_processor_logger,
            Some(scorer),
        );

        // try to connect to peers we already have a channel with
        let connect_peer_man = peer_man.clone();
        let connect_persister = persister.clone();
        let connect_proxy = websocket_proxy_addr.clone();
        let connect_logger = logger.clone();
        spawn_local(async move {
            loop {
                connect_logger.log(&Record::new(
                    lightning::util::logger::Level::Debug,
                    format_args!("DEBUG: about to list peer connections"),
                    "node",
                    "",
                    0,
                ));
                let peer_connections = connect_persister.list_peer_connection_info();
                let current_connections = connect_peer_man.get_peer_node_ids();

                let not_connected: Vec<&(PublicKey, String)> = peer_connections
                    .iter()
                    .filter(|(p, _)| !current_connections.contains(p))
                    .collect();

                for (pubkey, conn_str) in not_connected.iter() {
                    connect_logger.log(&Record::new(
                        lightning::util::logger::Level::Debug,
                        format_args!("DEBUG: going to auto connect to peer: {}", pubkey),
                        "node",
                        "",
                        0,
                    ));
                    let conn = conn_str.parse::<SocketAddr>();
                    if conn.is_err() {
                        continue;
                    }
                    match connect_peer(
                        connect_proxy.clone(),
                        pubkey.to_owned(),
                        conn.unwrap(),
                        connect_peer_man.clone(),
                    )
                    .await
                    {
                        Ok(_) => {
                            connect_logger.log(&Record::new(
                                lightning::util::logger::Level::Debug,
                                format_args!("DEBUG: auto connected peer: {}", pubkey),
                                "node",
                                "",
                                0,
                            ));
                        }
                        Err(e) => {
                            connect_logger.log(&Record::new(
                                lightning::util::logger::Level::Warn,
                                format_args!("WARN: could not auto connect peer: {}", e),
                                "node",
                                "",
                                0,
                            ));
                        }
                    }
                }
                sleep(5 * 1000).await;
            }
        });

        Ok(Node {
            uuid: node_index.uuid,
            pubkey,
            peer_manager: peer_man,
            keys_manager,
            chain,
            channel_manager,
            chain_monitor,
            invoice_payer,
            network,
            persister,
            _background_processor: background_processor,
            logger,
            websocket_proxy_addr,
            temp_peer_connection_map: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub async fn connect_peer(&self, peer_pubkey_and_ip_addr: String) -> Result<(), MutinyError> {
        if peer_pubkey_and_ip_addr.is_empty() {
            return Err(MutinyError::PeerInfoParseFailed)
                .context("connect_peer requires peer connection info")?;
        };
        let (pubkey, peer_addr) = match parse_peer_info(peer_pubkey_and_ip_addr) {
            Ok(info) => info,
            Err(e) => {
                return Err(MutinyError::PeerInfoParseFailed)
                    .with_context(|| format!("could not parse peer info: {}", e))?;
            }
        };

        if connect_peer_if_necessary(
            self.websocket_proxy_addr.clone(),
            pubkey,
            peer_addr,
            self.peer_manager.clone(),
        )
        .await
        .is_err()
        {
            Err(MutinyError::PeerInfoParseFailed)
                .with_context(|| format!("could not connect to peer: {pubkey}"))?
        } else {
            // store this so we can save later if needed
            self.temp_peer_connection_map
                .lock()
                .unwrap()
                .insert(pubkey.to_string(), peer_addr.to_string());
            Ok(())
        }
    }

    pub fn get_phantom_route_hint(&self) -> PhantomRouteHints {
        self.channel_manager.get_phantom_route_hints()
    }

    pub fn create_phantom_invoice(
        &self,
        amount_sat: Option<u64>,
        description: String,
        route_hints: Vec<PhantomRouteHints>,
    ) -> Result<Invoice, MutinyError> {
        let amount_msat = amount_sat.map(|s| s * 1_000);
        let invoice = match create_phantom_invoice::<InMemorySigner, Arc<PhantomKeysManager>>(
            amount_msat,
            None,
            description,
            1500,
            route_hints,
            self.keys_manager.clone(),
            currency_from_network(self.network),
        ) {
            Ok(inv) => {
                self.logger.log(&Record::new(
                    lightning::util::logger::Level::Info,
                    format_args!(
                        "SUCCESS: generated invoice: {} with amount {:?}",
                        inv, amount_msat
                    ),
                    "node",
                    "",
                    0,
                ));
                inv
            }
            Err(e) => {
                self.logger.log(&Record::new(
                    lightning::util::logger::Level::Error,
                    format_args!("ERROR: could not generate invoice: {}", e),
                    "node",
                    "",
                    0,
                ));
                return Err(MutinyError::InvoiceCreationFailed);
            }
        };

        let last_update = crate::utils::now().as_secs();
        let payment_hash = PaymentHash(invoice.payment_hash().into_inner());
        let payment_info = PaymentInfo {
            preimage: None,
            secret: Some(invoice.payment_secret().0),
            status: HTLCStatus::Pending,
            amt_msat: MillisatAmount(amount_msat),
            fee_paid_msat: None,
            bolt11: Some(invoice.to_string()),
            last_update,
        };
        match self
            .persister
            .persist_payment_info(payment_hash, payment_info, true)
        {
            Ok(_) => (),
            Err(e) => {
                self.logger.log(&Record::new(
                    lightning::util::logger::Level::Error,
                    format_args!("ERROR: could not persist payment info: {}", e),
                    "node",
                    "",
                    0,
                ));
            }
        }
        Ok(invoice)
    }

    pub fn get_invoice(&self, invoice: Invoice) -> Result<MutinyInvoice, MutinyError> {
        let payment_hash = invoice.payment_hash();
        let (payment_info, inbound) = self.get_payment_info_from_persisters(payment_hash)?;
        let mut mutiny_invoice: MutinyInvoice = invoice.into();
        mutiny_invoice.is_send = !inbound;
        mutiny_invoice.paid = matches!(payment_info.status, HTLCStatus::Succeeded);
        Ok(mutiny_invoice)
    }

    pub fn list_invoices(&self) -> Result<Vec<MutinyInvoice>, MutinyError> {
        let mut inbound_invoices = self.list_payment_info_from_persisters(true);
        let mut outbound_invoices = self.list_payment_info_from_persisters(false);
        inbound_invoices.append(&mut outbound_invoices);
        Ok(inbound_invoices)
    }

    fn list_payment_info_from_persisters(&self, inbound: bool) -> Vec<MutinyInvoice> {
        self.persister
            .list_payment_info(inbound)
            .into_iter()
            .filter_map(|(h, i)| match i.bolt11 {
                Some(bolt11) => {
                    // Construct an invoice from a bolt11, easy
                    let invoice_res = Invoice::from_str(&bolt11).map_err(Into::<MutinyError>::into);
                    match invoice_res {
                        Ok(invoice) => {
                            let mut mutiny_invoice: MutinyInvoice = invoice.clone().into();
                            mutiny_invoice.is_send = !inbound;
                            mutiny_invoice.paid = matches!(i.status, HTLCStatus::Succeeded);
                            mutiny_invoice.amount_sats =
                                if let Some(inv_amt) = invoice.amount_milli_satoshis() {
                                    if inv_amt == 0 {
                                        i.amt_msat.0.map(|a| a / 1_000)
                                    } else {
                                        Some(inv_amt / 1_000)
                                    }
                                } else {
                                    i.amt_msat.0.map(|a| a / 1_000)
                                };
                            Some(mutiny_invoice)
                        }
                        Err(_) => None,
                    }
                }
                None => {
                    // Constructing MutinyInvoice from no invoice, harder
                    let paid = matches!(i.status, HTLCStatus::Succeeded);
                    let sats: Option<u64> = i.amt_msat.0.map(|s| s / 1_000);
                    let fee_paid_sat = i.fee_paid_msat.map(|f| f / 1_000);
                    let preimage = i.preimage.map(|p| p.to_hex());
                    Some(MutinyInvoice::new(
                        None,
                        None,
                        h,
                        preimage,
                        None,
                        sats,
                        i.last_update,
                        paid,
                        fee_paid_sat,
                        !inbound,
                    ))
                }
            })
            .collect()
    }

    fn get_payment_info_from_persisters(
        &self,
        payment_hash: &bitcoin_hashes::sha256::Hash,
    ) -> Result<(PaymentInfo, bool), MutinyError> {
        // try inbound first
        if let Some(payment_info) = self.persister.read_payment_info(
            PaymentHash(payment_hash.into_inner()),
            true,
            self.logger.clone(),
        ) {
            return Ok((payment_info, true));
        }

        // if no inbound check outbound
        match self.persister.read_payment_info(
            PaymentHash(payment_hash.into_inner()),
            false,
            self.logger.clone(),
        ) {
            Some(payment_info) => Ok((payment_info, false)),
            None => Err(MutinyError::InvoiceInvalid),
        }
    }

    /// pay_invoice sends off the payment but does not wait for results
    pub fn pay_invoice(
        &self,
        invoice: Invoice,
        amt_sats: Option<u64>,
    ) -> Result<MutinyInvoice, MutinyError> {
        let (pay_result, amt_msat) = if invoice.amount_milli_satoshis().is_none() {
            if amt_sats.is_none() {
                return Err(MutinyError::InvoiceInvalid);
            }
            (
                self.invoice_payer
                    .pay_zero_value_invoice(&invoice, amt_sats.unwrap()),
                amt_sats.unwrap() * 1_000,
            )
        } else {
            if amt_sats.is_some() {
                return Err(MutinyError::InvoiceInvalid);
            }
            (
                self.invoice_payer.pay_invoice(&invoice),
                invoice.amount_milli_satoshis().unwrap(),
            )
        };

        let last_update = crate::utils::now().as_secs();
        let mut payment_info = PaymentInfo {
            preimage: None,
            secret: None,
            status: HTLCStatus::Pending,
            amt_msat: MillisatAmount(Some(amt_msat)),
            fee_paid_msat: None,
            bolt11: Some(invoice.to_string()),
            last_update,
        };
        self.persister.persist_payment_info(
            PaymentHash(invoice.payment_hash().into_inner()),
            payment_info.clone(),
            false,
        )?;

        match pay_result {
            Ok(_) => {
                let mut mutiny_invoice: MutinyInvoice = invoice.into();
                mutiny_invoice.paid = false;
                mutiny_invoice.is_send = true;
                Ok(mutiny_invoice)
            }
            Err(_) => {
                payment_info.status = HTLCStatus::Failed;
                self.persister.persist_payment_info(
                    PaymentHash(invoice.payment_hash().into_inner()),
                    payment_info,
                    false,
                )?;
                Err(MutinyError::RoutingFailed)
            }
        }
    }

    /// keysend sends off the payment but does not wait for results
    pub fn keysend(&self, to_node: PublicKey, amt_sats: u64) -> Result<MutinyInvoice, MutinyError> {
        let mut entropy = [0u8; 32];
        getrandom::getrandom(&mut entropy).map_err(|_| MutinyError::SeedGenerationFailed)?;
        let preimage = PaymentPreimage(entropy);

        let amt_msats = amt_sats * 1000;

        let pay_result = self
            .invoice_payer
            .pay_pubkey(to_node, preimage, amt_msats, 40);

        let payment_hash = PaymentHash(Sha256::hash(&preimage.0).into_inner());

        let last_update = crate::utils::now().as_secs();
        let mut payment_info = PaymentInfo {
            preimage: Some(preimage.0),
            secret: None,
            status: HTLCStatus::Pending,
            amt_msat: MillisatAmount(Some(amt_msats)),
            fee_paid_msat: None,
            bolt11: None,
            last_update,
        };

        self.persister
            .persist_payment_info(payment_hash, payment_info.clone(), false)?;

        match pay_result {
            Ok(_) => {
                let mutiny_invoice: MutinyInvoice = MutinyInvoice::new(
                    None,
                    None,
                    payment_hash.0.to_hex(),
                    Some(preimage.0.to_hex()),
                    Some(to_node.to_hex()),
                    Some(amt_sats),
                    payment_info.last_update,
                    false,
                    None,
                    true,
                );
                Ok(mutiny_invoice)
            }
            Err(_) => {
                payment_info.status = HTLCStatus::Failed;
                self.persister
                    .persist_payment_info(payment_hash, payment_info, false)?;
                Err(MutinyError::RoutingFailed)
            }
        }
    }

    pub async fn open_channel(
        &self,
        pubkey: PublicKey,
        amount_sat: u64,
    ) -> Result<[u8; 32], MutinyError> {
        let config = default_user_config();
        match self
            .channel_manager
            .create_channel(pubkey, amount_sat, 0, 0, Some(config))
        {
            Ok(res) => {
                self.logger.log(&Record::new(
                    lightning::util::logger::Level::Info,
                    format_args!("SUCCESS: channel initiated with peer: {:?}", pubkey),
                    "node",
                    "",
                    0,
                ));

                // persist the peer channel info so we can connect later
                if let Some(conn_str) = self
                    .temp_peer_connection_map
                    .lock()
                    .unwrap()
                    .get(&pubkey.to_string())
                {
                    if self
                        .persister
                        .persist_peer_connection_info(pubkey.to_string(), conn_str.clone())
                        .is_err()
                    {
                        self.logger.log(&Record::new(
                            lightning::util::logger::Level::Warn,
                            format_args!("WARN: could not store peer connection info",),
                            "node",
                            "",
                            0,
                        ));
                    }
                }

                Ok(res)
            }
            Err(e) => {
                self.logger.log(&Record::new(
                    lightning::util::logger::Level::Error,
                    format_args!(
                        "ERROR: failed to open channel to pubkey {:?}: {:?}",
                        pubkey, e
                    ),
                    "node",
                    "",
                    0,
                ));
                Err(MutinyError::ChannelCreationFailed)
            }
        }
    }
}

pub(crate) async fn connect_peer_if_necessary(
    websocket_proxy_addr: String,
    pubkey: PublicKey,
    peer_addr: SocketAddr,
    peer_manager: Arc<PeerManager>,
) -> Result<(), MutinyError> {
    for node_pubkey in peer_manager.get_peer_node_ids() {
        if node_pubkey == pubkey {
            return Ok(());
        }
    }

    connect_peer(websocket_proxy_addr, pubkey, peer_addr, peer_manager).await
}
pub(crate) async fn connect_peer(
    websocket_proxy_addr: String,
    pubkey: PublicKey,
    peer_addr: SocketAddr,
    peer_manager: Arc<PeerManager>,
) -> Result<(), MutinyError> {
    // first make a connection to the node
    let tcp_proxy = Arc::new(TcpProxy::new(websocket_proxy_addr, peer_addr).await);
    let mut descriptor = SocketDescriptor::new(tcp_proxy);

    // then give that connection to the peer manager
    let initial_bytes = peer_manager.new_outbound_connection(
        pubkey,
        descriptor.clone(),
        Some(get_net_addr_from_socket(peer_addr)),
    )?;

    let sent_bytes = descriptor.send_data(&initial_bytes, true);
    trace!("sent {sent_bytes} to node: {pubkey}");

    // schedule a reader on the connection
    let mut new_descriptor = descriptor.clone();
    spawn_local(async move {
        while let Some(msg) = descriptor.conn.read.lock().await.next().await {
            if let Ok(msg_contents) = msg {
                match msg_contents {
                    Message::Text(t) => {
                        warn!(
                            "received text from websocket when we should only receive binary: {}",
                            t
                        )
                    }
                    Message::Bytes(b) => {
                        trace!("received binary data from websocket");

                        let read_res = peer_manager.read_event(&mut new_descriptor, &b);
                        match read_res {
                            // TODO handle read boolean event
                            Ok(_read_bool) => {
                                trace!("read event from the node");
                                peer_manager.process_events();
                            }
                            Err(e) => error!("got an error reading event: {}", e),
                        }
                    }
                };
            }
        }

        // TODO when we detect an error, lock the writes and close connection.
        info!("WebSocket Closed")
    });

    Ok(())
}

fn get_net_addr_from_socket(socket_addr: SocketAddr) -> NetAddress {
    match socket_addr {
        SocketAddr::V4(sockaddr) => NetAddress::IPv4 {
            addr: sockaddr.ip().octets(),
            port: sockaddr.port(),
        },
        SocketAddr::V6(sockaddr) => NetAddress::IPv6 {
            addr: sockaddr.ip().octets(),
            port: sockaddr.port(),
        },
    }
}

pub(crate) fn create_peer_manager(
    km: Arc<PhantomKeysManager>,
    lightning_msg_handler: MessageHandler,
    logger: Arc<MutinyLogger>,
) -> PeerManager {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut ephemeral_bytes = [0u8; 32];
    getrandom::getrandom(&mut ephemeral_bytes).expect("Failed to generate entropy");

    PeerManager::new(
        lightning_msg_handler,
        km.get_node_secret(Recipient::Node)
            .expect("Failed to get node secret"),
        now as u32,
        &ephemeral_bytes,
        logger,
        Arc::new(IgnoringMessageHandler {}),
    )
}

pub(crate) fn parse_peer_info(
    peer_pubkey_and_ip_addr: String,
) -> Result<(PublicKey, SocketAddr), MutinyError> {
    let mut pubkey_and_addr = peer_pubkey_and_ip_addr.split('@');
    let pubkey = pubkey_and_addr.next();
    let peer_addr_str = match pubkey_and_addr.next() {
        None => {
            return Err(MutinyError::PeerInfoParseFailed).context(
                "incorrectly formatted peer info. Should be formatted as: `pubkey@host:port`",
            )?
        }
        Some(str) => str,
    };

    let peer_addr_str_with_port = if peer_addr_str.contains(":") {
        peer_addr_str.to_string()
    } else {
        format!("{peer_addr_str}:9735")
    };

    let peer_addr = peer_addr_str_with_port
        .to_socket_addrs()
        .map(|mut r| r.next());
    if peer_addr.is_err() || peer_addr.as_ref().unwrap().is_none() {
        return Err(MutinyError::PeerInfoParseFailed)
            .context("couldn't parse pubkey@host:port into a socket address")?;
    }

    let pubkey = PublicKey::from_str(pubkey.unwrap());
    if pubkey.is_err() {
        return Err(MutinyError::PeerInfoParseFailed)
            .context("unable to parse given pubkey for node")?;
    }

    Ok((pubkey.unwrap(), peer_addr.unwrap().unwrap()))
}

pub(crate) fn default_user_config() -> UserConfig {
    UserConfig {
        channel_handshake_limits: ChannelHandshakeLimits {
            // lnd's max to_self_delay is 2016, so we want to be compatible.
            their_to_self_delay: 2016,
            ..Default::default()
        },
        channel_handshake_config: ChannelHandshakeConfig {
            minimum_depth: 1,
            announced_channel: false,
            negotiate_scid_privacy: true,
            ..Default::default()
        },
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use crate::test::*;
    use std::{net::SocketAddr, str::FromStr};

    use crate::node::parse_peer_info;

    use secp256k1::PublicKey;
    use wasm_bindgen_test::{wasm_bindgen_test as test, wasm_bindgen_test_configure};

    wasm_bindgen_test_configure!(run_in_browser);

    #[test]
    async fn test_parse_peer_info() {
        log!("test parse peer info");

        let pub_key = PublicKey::from_str(
            "0218845781f631c48f1c9709e23092067d06837f30aa0cd0544ac887fe91ddd166",
        )
        .unwrap();
        let addr = "127.0.0.1:4000";

        let (peer_pubkey, peer_addr) = parse_peer_info(format!("{}@{addr}", pub_key)).unwrap();

        assert_eq!(pub_key, peer_pubkey);
        assert_eq!(addr.parse::<SocketAddr>().unwrap(), peer_addr);
    }

    #[test]
    async fn test_parse_peer_info_no_port() {
        log!("test parse peer info with no port");

        let pub_key = PublicKey::from_str(
            "0218845781f631c48f1c9709e23092067d06837f30aa0cd0544ac887fe91ddd166",
        )
        .unwrap();
        let addr = "127.0.0.1";
        let port = "9735";

        let (peer_pubkey, peer_addr) = parse_peer_info(format!("{pub_key}@{addr}")).unwrap();

        assert_eq!(pub_key, peer_pubkey);
        assert_eq!(
            format!("{addr}:{port}").parse::<SocketAddr>().unwrap(),
            peer_addr
        );
    }
}
