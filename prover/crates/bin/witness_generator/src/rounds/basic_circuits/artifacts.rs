use std::time::Instant;

use async_trait::async_trait;
use zksync_object_store::ObjectStore;
use zksync_prover_dal::{ConnectionPool, Prover, ProverDal};
use zksync_prover_fri_types::AuxOutputWitnessWrapper;
use zksync_prover_fri_utils::get_recursive_layer_circuit_id_for_base_layer;
use zksync_prover_interface::{inputs::WitnessInputData, Bincode};
use zksync_types::{basic_fri_types::AggregationRound, L1BatchNumber};

use crate::{
    artifacts::ArtifactsManager,
    rounds::basic_circuits::{
        utils::create_aggregation_jobs, BasicCircuitArtifacts, BasicCircuits,
        BasicWitnessGeneratorJob,
    },
    utils::SchedulerPartialInputWrapper,
};

#[async_trait]
impl ArtifactsManager for BasicCircuits {
    type InputMetadata = L1BatchNumber;
    type InputArtifacts = BasicWitnessGeneratorJob;
    type OutputArtifacts = BasicCircuitArtifacts;
    type BlobUrls = String;

    async fn get_artifacts(
        metadata: &Self::InputMetadata,
        object_store: &dyn ObjectStore,
    ) -> anyhow::Result<Self::InputArtifacts> {
        let l1_batch_number = *metadata;
        let data: WitnessInputData = match object_store.get(l1_batch_number).await {
            Ok(data) => data,
            Err(err_cbor) => object_store
                .get::<WitnessInputData<Bincode>>(l1_batch_number)
                .await
                .map(Into::into)
                .map_err(|err_bincode| anyhow::anyhow!("Getting data with bincode failed with {err_bincode}, getting data with CBOR failed with {err_cbor}"))?,
        };

        Ok(BasicWitnessGeneratorJob {
            block_number: l1_batch_number,
            data,
        })
    }

    async fn save_to_bucket(
        job_id: u32,
        artifacts: Self::OutputArtifacts,
        object_store: &dyn ObjectStore,
    ) -> String {
        let aux_output_witness_wrapper =
            AuxOutputWitnessWrapper(artifacts.aux_output_witness.clone());

        object_store
            .put(L1BatchNumber(job_id), &aux_output_witness_wrapper)
            .await
            .unwrap();
        let wrapper = SchedulerPartialInputWrapper(artifacts.scheduler_witness);
        object_store
            .put(L1BatchNumber(job_id), &wrapper)
            .await
            .unwrap()
    }

    #[tracing::instrument(skip_all, fields(l1_batch = %job_id))]
    async fn save_to_database(
        connection_pool: &ConnectionPool<Prover>,
        job_id: u32,
        started_at: Instant,
        blob_urls: String,
        artifacts: Self::OutputArtifacts,
    ) -> anyhow::Result<()> {
        let mut connection = connection_pool
            .connection()
            .await
            .expect("failed to get database connection");
        let mut transaction = connection
            .start_transaction()
            .await
            .expect("failed to get database transaction");
        let protocol_version_id = transaction
            .fri_basic_witness_generator_dal()
            .protocol_version_for_l1_batch(L1BatchNumber(job_id))
            .await;
        let batch_sealed_at = transaction
            .fri_basic_witness_generator_dal()
            .get_batch_sealed_at_timestamp(L1BatchNumber(job_id))
            .await;

        transaction
            .fri_prover_jobs_dal()
            .insert_prover_jobs(
                L1BatchNumber(job_id),
                artifacts.circuit_urls,
                AggregationRound::BasicCircuits,
                0,
                protocol_version_id,
                batch_sealed_at,
            )
            .await;

        create_aggregation_jobs(
            &mut transaction,
            L1BatchNumber(job_id),
            &artifacts.queue_urls,
            &blob_urls,
            get_recursive_layer_circuit_id_for_base_layer,
            protocol_version_id,
        )
        .await
        .unwrap();

        transaction
            .fri_basic_witness_generator_dal()
            .mark_witness_job_as_successful(L1BatchNumber(job_id), started_at.elapsed())
            .await;
        transaction
            .commit()
            .await
            .expect("failed to commit database transaction");
        Ok(())
    }
}
