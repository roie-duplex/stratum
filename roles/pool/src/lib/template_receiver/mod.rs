use super::{
    error::{PoolError, PoolResult},
    mining_pool::{EitherFrame, StdFrame},
    status,
};
use async_channel::{Receiver, Sender};
use codec_sv2::{HandshakeRole, Initiator};
use error_handling::handle_result;
use key_utils::Secp256k1PublicKey;
use network_helpers_sv2::noise_connection::Connection;
use roles_logic_sv2::{
    handlers::template_distribution::ParseTemplateDistributionMessagesFromServer,
    parsers::{AnyMessage, TemplateDistribution},
    template_distribution_sv2::{
        CoinbaseOutputConstraints, NewTemplate, SetNewPrevHash, SubmitSolution,
    },
    utils::Mutex,
};
use std::{convert::TryInto, net::SocketAddr, sync::Arc};
use tokio::{net::TcpStream, task};
use tracing::{info, warn};

mod message_handler;
mod setup_connection;
use setup_connection::SetupConnectionHandler;

pub struct TemplateRx {
    receiver: Receiver<EitherFrame>,
    sender: Sender<EitherFrame>,
    message_received_signal: Receiver<()>,
    new_template_sender: Sender<NewTemplate<'static>>,
    new_prev_hash_sender: Sender<SetNewPrevHash<'static>>,
    status_tx: status::Sender,
}

impl TemplateRx {
    #[allow(clippy::too_many_arguments)]
    pub async fn connect(
        address: SocketAddr,
        templ_sender: Sender<NewTemplate<'static>>,
        prev_h_sender: Sender<SetNewPrevHash<'static>>,
        solution_receiver: Receiver<SubmitSolution<'static>>,
        message_received_signal: Receiver<()>,
        status_tx: status::Sender,
        coinbase_out_len: u32,
        coinbase_out_sigops: u16,
        expected_tp_authority_public_key: Option<Secp256k1PublicKey>,
    ) -> PoolResult<()> {
        let stream = loop {
            match TcpStream::connect(address).await {
                Ok(stream) => break stream,
                Err(err) => {
                    warn!("Failed to connect to {}: {}. Retrying...", address, err);
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        };
        info!("Connected to template distribution server at {}", address);

        let initiator = match expected_tp_authority_public_key {
            Some(expected_tp_authority_public_key) => {
                Initiator::from_raw_k(expected_tp_authority_public_key.into_bytes())
            }
            None => Initiator::without_pk(),
        }?;
        let (mut receiver, mut sender) =
            Connection::new(stream, HandshakeRole::Initiator(initiator))
                .await
                .unwrap();

        SetupConnectionHandler::setup(&mut receiver, &mut sender, address).await?;

        let self_ = Arc::new(Mutex::new(Self {
            receiver,
            sender,
            new_template_sender: templ_sender,
            new_prev_hash_sender: prev_h_sender,
            message_received_signal,
            status_tx,
        }));
        let cloned = self_.clone();

        let coinbase_output_constraints = CoinbaseOutputConstraints {
            coinbase_output_max_additional_size: coinbase_out_len,
            coinbase_output_max_additional_sigops: coinbase_out_sigops,
        };
        let frame = AnyMessage::TemplateDistribution(
            TemplateDistribution::CoinbaseOutputConstraints(coinbase_output_constraints),
        )
        .try_into()?;

        Self::send(self_.clone(), frame).await?;

        task::spawn(async { Self::start(cloned).await });
        task::spawn(async { Self::on_new_solution(self_, solution_receiver).await });

        Ok(())
    }

    pub async fn start(self_: Arc<Mutex<Self>>) {
        let (recv_msg_signal, receiver, new_template_sender, new_prev_hash_sender, status_tx) =
            self_
                .safe_lock(|s| {
                    (
                        s.message_received_signal.clone(),
                        s.receiver.clone(),
                        s.new_template_sender.clone(),
                        s.new_prev_hash_sender.clone(),
                        s.status_tx.clone(),
                    )
                })
                .unwrap();
        loop {
            let message_from_tp = handle_result!(status_tx, receiver.recv().await);
            let mut message_from_tp: StdFrame = handle_result!(
                status_tx,
                message_from_tp
                    .try_into()
                    .map_err(|e| PoolError::Codec(codec_sv2::Error::FramingSv2Error(e)))
            );
            let message_type_res = message_from_tp
                .get_header()
                .ok_or_else(|| PoolError::Custom(String::from("No header set")));
            let message_type = handle_result!(status_tx, message_type_res).msg_type();
            let payload = message_from_tp.payload();
            let msg = handle_result!(
                status_tx,
                ParseTemplateDistributionMessagesFromServer::handle_message_template_distribution(
                    self_.clone(),
                    message_type,
                    payload,
                )
            );
            match msg {
                roles_logic_sv2::handlers::SendTo_::RelayNewMessageToRemote(_, m) => match m {
                    TemplateDistribution::CoinbaseOutputConstraints(_) => todo!(),
                    TemplateDistribution::NewTemplate(m) => {
                        let res = new_template_sender.send(m).await;
                        handle_result!(status_tx, res);
                        handle_result!(status_tx, recv_msg_signal.recv().await);
                    }
                    TemplateDistribution::RequestTransactionData(_) => todo!(),
                    TemplateDistribution::RequestTransactionDataError(_) => todo!(),
                    TemplateDistribution::RequestTransactionDataSuccess(_) => todo!(),
                    TemplateDistribution::SetNewPrevHash(m) => {
                        let res = new_prev_hash_sender.send(m).await;
                        handle_result!(status_tx, res);
                        handle_result!(status_tx, recv_msg_signal.recv().await);
                    }
                    TemplateDistribution::SubmitSolution(_) => todo!(),
                },
                roles_logic_sv2::handlers::SendTo_::None(None) => (),
                _ => {
                    info!("Error: {:?}", msg);
                    std::process::abort();
                }
            }
        }
    }

    pub async fn send(self_: Arc<Mutex<Self>>, sv2_frame: StdFrame) -> PoolResult<()> {
        let either_frame = sv2_frame.into();
        let sender = self_
            .safe_lock(|self_| self_.sender.clone())
            .map_err(|e| PoolError::PoisonLock(e.to_string()))?;
        sender.send(either_frame).await?;
        Ok(())
    }

    async fn on_new_solution(self_: Arc<Mutex<Self>>, rx: Receiver<SubmitSolution<'static>>) {
        let status_tx = self_.safe_lock(|s| s.status_tx.clone()).unwrap();
        while let Ok(solution) = rx.recv().await {
            info!("Sending Solution to TP: {:?}", &solution);
            let sv2_frame_res: Result<StdFrame, _> =
                AnyMessage::TemplateDistribution(TemplateDistribution::SubmitSolution(solution))
                    .try_into();
            match sv2_frame_res {
                Ok(frame) => {
                    handle_result!(status_tx, Self::send(self_.clone(), frame).await);
                }
                Err(_e) => {
                    // return submit error
                    todo!()
                }
            };
        }
    }
}
