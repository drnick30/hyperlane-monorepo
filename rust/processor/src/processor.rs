use async_trait::async_trait;
use color_eyre::{
    eyre::{eyre, WrapErr},
    Result,
};
use futures_util::future::select_all;
use std::{collections::HashMap, sync::Arc};
use tokio::{
    sync::{oneshot::channel, RwLock},
    task::JoinHandle,
    time::sleep,
};
use tracing::{info, instrument, Instrument};

use optics_base::{
    agent::{AgentCore, OpticsAgent},
    cancel_task, decl_agent,
    home::Homes,
    replica::Replicas,
    reset_loop_if,
};
use optics_core::{
    accumulator::merkle::Proof,
    traits::{CommittedMessage, Common, Home, MessageStatus, Replica},
};

use crate::{
    prover::{Prover, ProverSync},
    settings::Settings,
};

#[derive(Debug)]
pub(crate) struct ReplicaProcessor {
    interval_seconds: u64,
    replica: Arc<Replicas>,
    home: Arc<Homes>,
    prover: Arc<RwLock<Prover>>,
}

impl ReplicaProcessor {
    pub(crate) fn new(
        interval_seconds: u64,
        replica: Arc<Replicas>,
        home: Arc<Homes>,
        prover: Arc<RwLock<Prover>>,
    ) -> Self {
        Self {
            interval_seconds,
            replica,
            home,
            prover,
        }
    }

    #[instrument]
    pub(crate) fn spawn(self) -> JoinHandle<Result<()>> {
        tokio::spawn(async move {
            info!("Starting processor");
            let domain = self.replica.local_domain();
            let interval = self.interval_seconds;

            // The basic structure of this loop is as follows:
            // 1. Get the last processed index
            // 2. Check if the Home knows of a message above that index
            //      - If not, wait and poll again
            // 3. Check if we have a proof for that message
            //      - If not, wait and poll again
            // 4. Submit the proof to the replica
            loop {
                let sequence = self.replica.next_to_process().await?;
                info!(
                    "Next to process for replica {} is {}",
                    self.replica.name(),
                    sequence
                );

                let message = self.home.message_by_sequence(domain, sequence).await?;
                reset_loop_if!(
                    message.is_none(),
                    interval,
                    "Home does not contain message at {}:{}",
                    domain,
                    sequence
                );

                let message = message.unwrap();

                // Lock is dropped immediately
                let proof_res = self.prover.read().await.prove(message.leaf_index as usize);
                reset_loop_if!(
                    proof_res.is_err(),
                    interval,
                    "Prover does not contain leaf at index {}",
                    message.leaf_index
                );

                let proof = proof_res.unwrap();
                if proof.leaf != message.to_leaf() {
                    let err = format!("Leaf in prover does not match retrieved message. Index: {}. Calculated: {}. Prover: {}.", message.leaf_index, message.to_leaf(), proof.leaf);
                    tracing::error!("{}", err);
                    color_eyre::eyre::bail!(err);
                }

                // Dispatch for processing
                info!(
                    "Dispatching a message for processing {}:{}",
                    domain, sequence
                );
                self.process(message, proof).await?;

                sleep(std::time::Duration::from_secs(interval)).await;
            }
        }.in_current_span())
    }

    #[instrument(err)]
    /// Dispatch a message for processing. If the message is already proven, process only.
    async fn process(&self, message: CommittedMessage, proof: Proof) -> Result<()> {
        let status = self.replica.message_status(message.to_leaf()).await?;

        match status {
            MessageStatus::None => {
                self.replica
                    .prove_and_process(message.as_ref(), &proof)
                    .await?;
            }
            MessageStatus::Pending => {
                self.replica.process(message.as_ref()).await?;
            }
            MessageStatus::Processed => {} // Indicates race condition?
        }

        Ok(())
    }
}

decl_agent!(
    /// A processor agent
    Processor {
        interval_seconds: u64,
        prover: Arc<RwLock<Prover>>,
        replica_tasks: RwLock<HashMap<String, JoinHandle<Result<()>>>>,
    }
);

impl Processor {
    /// Instantiate a new processor
    pub fn new(interval_seconds: u64, core: AgentCore) -> Self {
        Self {
            interval_seconds,
            prover: Arc::new(RwLock::new(Prover::from_disk(&core.db))),
            core,
            replica_tasks: Default::default(),
        }
    }
}

#[async_trait]
#[allow(clippy::unit_arg)]
impl OpticsAgent for Processor {
    type Settings = Settings;

    async fn from_settings(settings: Self::Settings) -> Result<Self>
    where
        Self: Sized,
    {
        Ok(Self::new(
            settings.polling_interval,
            settings.as_ref().try_into_core().await?,
        ))
    }

    fn run(&self, name: &str) -> JoinHandle<Result<()>> {
        let home = self.home();
        let prover = self.prover.clone();
        let interval_seconds = self.interval_seconds;

        let replica_opt = self.replica_by_name(name);
        let name = name.to_owned();

        tokio::spawn(async move {
            let replica = replica_opt.ok_or_else(|| eyre!("No replica named {}", name))?;
            ReplicaProcessor::new(interval_seconds, replica, home, prover)
                .spawn()
                .await?
        })
    }

    #[tracing::instrument(err)]
    async fn run_many(&self, replicas: &[&str]) -> Result<()> {
        let (_tx, rx) = channel();
        let interval_seconds = self.interval_seconds;

        info!("Starting ProverSync task");
        let sync = ProverSync::new(self.prover.clone(), self.home(), self.db(), rx);
        let sync_task = tokio::spawn(async move {
            sync.poll_updates(interval_seconds)
                .await
                .wrap_err("ProverSync task has shut down")
        });

        // for each specified replica, spawn a joinable task
        let mut handles: Vec<_> = replicas.iter().map(|name| self.run(name)).collect();

        handles.push(sync_task);

        // The first time a task fails we cancel all other tasks
        let (res, _, remaining) = select_all(handles).await;
        for task in remaining {
            cancel_task!(task);
        }

        res?
    }
}
