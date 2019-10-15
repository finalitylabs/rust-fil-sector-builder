use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;

use filecoin_proofs::error::ExpectWithBacktrace;
use storage_proofs::sector::SectorId;

use crate::error::Result;
use crate::kv_store::KeyValueStore;
use crate::metadata::{SealStatus, StagedSectorMetadata};
use crate::scheduler::SchedulerTask::OnSealMultipleComplete;
use crate::store::SectorStore;
use crate::worker::WorkerTask;
use crate::{
    GetSealedSectorResult, SealTicket, SealedSectorMetadata, SecondsSinceEpoch,
    SectorMetadataManager, UnpaddedBytesAmount,
};
use std::io::Read;

const FATAL_NORECV: &str = "could not receive task";
const FATAL_NOSEND: &str = "could not send";

pub struct Scheduler {
    pub thread: Option<thread::JoinHandle<()>>,
}

#[derive(Debug)]
pub struct PerformHealthCheck(pub bool);

#[derive(Debug)]
pub struct SealResult {
    pub sector_id: SectorId,
    pub sector_access: String,
    pub sector_path: PathBuf,
    pub seal_ticket: SealTicket,
    pub proofs_api_call_result: Result<filecoin_proofs::SealOutput>,
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum SchedulerTask<T: Read + Send> {
    AddPiece(
        String,
        u64,
        T,
        SecondsSinceEpoch,
        mpsc::SyncSender<Result<SectorId>>,
    ),
    GetSealedSectors(
        PerformHealthCheck,
        mpsc::SyncSender<Result<Vec<GetSealedSectorResult>>>,
    ),
    GetStagedSectors(mpsc::SyncSender<Result<Vec<StagedSectorMetadata>>>),
    GetSealStatus(SectorId, mpsc::SyncSender<Result<SealStatus>>),
    GeneratePoSt(
        Vec<[u8; 32]>,
        [u8; 32],      // seed
        Vec<SectorId>, // faults
        mpsc::SyncSender<Result<Vec<u8>>>,
    ),
    RetrievePiece(String, mpsc::SyncSender<Result<Vec<u8>>>),
    SealAllStagedSectors(
        SealTicket,
        mpsc::SyncSender<Result<Vec<SealedSectorMetadata>>>,
    ),
    ResumeSealSector(
        SectorId,
        mpsc::SyncSender<Result<Vec<SealedSectorMetadata>>>,
    ),
    SealSector(
        SectorId,
        SealTicket,
        mpsc::SyncSender<Result<Vec<SealedSectorMetadata>>>,
    ),
    OnSealMultipleComplete {
        output: Vec<SealResult>,
        caller_done_tx: mpsc::SyncSender<Result<Vec<SealedSectorMetadata>>>,
    },
    HandleRetrievePieceResult(
        Result<(UnpaddedBytesAmount, PathBuf)>,
        mpsc::SyncSender<Result<Vec<u8>>>,
    ),
    Shutdown,
}

struct TaskHandler<T: KeyValueStore, U: SectorStore, V: 'static + Send + std::io::Read> {
    m: SectorMetadataManager<T, U>,
    scheduler_tx: mpsc::SyncSender<SchedulerTask<V>>,
    worker_tx: mpsc::Sender<WorkerTask>,
}

impl<T: KeyValueStore, U: SectorStore, V: 'static + Send + std::io::Read> TaskHandler<T, U, V> {
    fn handle(&mut self, task: SchedulerTask<V>) -> bool {
        match task {
            SchedulerTask::AddPiece(key, amt, file, store_until, tx) => {
                match self.m.add_piece(key, amt, file, store_until) {
                    Ok(sector_id) => {
                        tx.send(Ok(sector_id)).expects(FATAL_NOSEND);
                    }
                    Err(err) => {
                        tx.send(Err(err)).expects(FATAL_NOSEND);
                    }
                }

                true
            }
            SchedulerTask::GetSealStatus(sector_id, tx) => {
                tx.send(self.m.get_seal_status(sector_id))
                    .expects(FATAL_NOSEND);

                true
            }
            SchedulerTask::RetrievePiece(piece_key, tx) => {
                match self.m.create_retrieve_piece_task_proto(piece_key) {
                    Ok(proto) => {
                        let scheduler_tx_c = self.scheduler_tx.clone();

                        self.worker_tx
                            .send(WorkerTask::from_unseal_proto(
                                proto,
                                Box::new(move |output| {
                                    scheduler_tx_c
                                        .send(SchedulerTask::HandleRetrievePieceResult(output, tx))
                                        .expects(FATAL_NOSEND)
                                }),
                            ))
                            .expects(FATAL_NOSEND);
                    }
                    Err(err) => {
                        tx.send(Err(err)).expects(FATAL_NOSEND);
                    }
                }

                true
            }
            SchedulerTask::GetSealedSectors(check_health, tx) => {
                tx.send(self.m.get_sealed_sectors_filtered(check_health.0, |_| true))
                    .expects(FATAL_NOSEND);

                true
            }
            SchedulerTask::GetStagedSectors(tx) => {
                tx.send(Ok(self
                    .m
                    .get_staged_sectors_filtered(|_| true)
                    .into_iter()
                    .cloned()
                    .collect()))
                    .expect(FATAL_NOSEND);

                true
            }
            SchedulerTask::SealAllStagedSectors(seal_ticket, tx) => {
                self.m.mark_all_sectors_for_sealing();

                let r_protos = self
                    .m
                    .create_seal_task_protos(seal_ticket, |x| x.seal_status.is_ready_for_sealing());

                match r_protos {
                    Ok(protos) => {
                        for p in &protos {
                            self.m
                                .commit_sector_to_ticket(p.sector_id, p.seal_ticket.clone());
                        }

                        let scheduler_tx_c = self.scheduler_tx.clone();

                        self.worker_tx
                            .send(WorkerTask::from_seal_protos(
                                protos,
                                Box::new(move |output| {
                                    scheduler_tx_c
                                        .send(OnSealMultipleComplete {
                                            output,
                                            caller_done_tx: tx,
                                        })
                                        .expects(FATAL_NOSEND)
                                }),
                            ))
                            .expects(FATAL_NOSEND);
                    }
                    Err(err) => {
                        tx.send(Err(err)).expects(FATAL_NOSEND);
                    }
                }

                true
            }
            SchedulerTask::ResumeSealSector(sector_id, tx) => {
                let r_protos = self.m.create_resume_seal_task_protos(|x| {
                    x.seal_status.is_paused() && x.sector_id == sector_id
                });

                match r_protos {
                    Ok(protos) => {
                        for p in &protos {
                            self.m
                                .commit_sector_to_ticket(p.sector_id, p.seal_ticket.clone());
                        }

                        let scheduler_tx_c = self.scheduler_tx.clone();

                        self.worker_tx
                            .send(WorkerTask::from_seal_protos(
                                protos,
                                Box::new(move |output| {
                                    scheduler_tx_c
                                        .send(OnSealMultipleComplete {
                                            output,
                                            caller_done_tx: tx,
                                        })
                                        .expects(FATAL_NOSEND)
                                }),
                            ))
                            .expects(FATAL_NOSEND);
                    }
                    Err(err) => {
                        tx.send(Err(err)).expects(FATAL_NOSEND);
                    }
                }

                true
            }
            SchedulerTask::SealSector(sector_id, seal_ticket, tx) => {
                self.m.mark_all_sectors_for_sealing();

                let r_protos = self.m.create_seal_task_protos(seal_ticket, |x| {
                    x.sector_id == sector_id && x.seal_status.is_ready_for_sealing()
                });

                match r_protos {
                    Ok(protos) => {
                        for p in &protos {
                            self.m
                                .commit_sector_to_ticket(p.sector_id, p.seal_ticket.clone());
                        }

                        let scheduler_tx_c = self.scheduler_tx.clone();

                        self.worker_tx
                            .send(WorkerTask::from_seal_protos(
                                protos,
                                Box::new(move |output| {
                                    scheduler_tx_c
                                        .send(OnSealMultipleComplete {
                                            output,
                                            caller_done_tx: tx,
                                        })
                                        .expects(FATAL_NOSEND)
                                }),
                            ))
                            .expects(FATAL_NOSEND);
                    }
                    Err(err) => {
                        tx.send(Err(err)).expects(FATAL_NOSEND);
                    }
                }

                true
            }
            SchedulerTask::OnSealMultipleComplete {
                output,
                caller_done_tx,
            } => {
                let r: Result<Vec<SealedSectorMetadata>> = output
                    .into_iter()
                    .map(|o| self.m.handle_seal_result(o))
                    .collect();

                caller_done_tx.send(r).expects(FATAL_NOSEND);

                true
            }
            SchedulerTask::HandleRetrievePieceResult(result, tx) => {
                tx.send(self.m.read_unsealed_bytes_from(result))
                    .expects(FATAL_NOSEND);

                true
            }
            SchedulerTask::GeneratePoSt(comm_rs, chg_seed, faults, tx) => {
                let proto = self
                    .m
                    .create_generate_post_task_proto(&comm_rs, &chg_seed, faults);

                let tx_c = tx.clone();

                self.worker_tx
                    .send(WorkerTask::from_generate_post_proto(
                        proto,
                        Box::new(move |r| tx_c.send(r).expects(FATAL_NOSEND)),
                    ))
                    .expects(FATAL_NOSEND);

                true
            }
            SchedulerTask::Shutdown => false,
        }
    }
}

impl Scheduler {
    #[allow(clippy::too_many_arguments)]
    pub fn start<
        T: 'static + KeyValueStore,
        S: 'static + SectorStore,
        U: 'static + std::io::Read + Send,
    >(
        scheduler_tx: mpsc::SyncSender<SchedulerTask<U>>,
        scheduler_rx: mpsc::Receiver<SchedulerTask<U>>,
        worker_tx: mpsc::Sender<WorkerTask>,
        m: SectorMetadataManager<T, S>,
    ) -> Result<Scheduler> {
        let mut handler = TaskHandler {
            m,
            scheduler_tx,
            worker_tx: worker_tx.clone(),
        };

        let thread = thread::spawn(move || loop {
            let task = scheduler_rx.recv().expects(FATAL_NORECV);
            if !handler.handle(task) {
                break;
            }
        });

        Ok(Scheduler {
            thread: Some(thread),
        })
    }
}
