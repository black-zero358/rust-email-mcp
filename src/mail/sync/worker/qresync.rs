//! Production QRESYNC turns. A connection is enabled and discarded in the same turn.
use super::{
    super::qresync,
    condstore::{EpochFloors, ScopeFloors},
    *,
};

impl Batch<'_> {
    pub(super) async fn qresync_turn(
        &self,
        session: Session,
        fence: Arc<ReconcileFence>,
        floors: ScopeFloors,
        preview: QresyncPreview,
    ) -> Result<Turn> {
        let enabled = match qresync::enable(session).await {
            Ok(enabled) => enabled,
            Err(_) => {
                return self
                    .condstore_fallback(fence, floors, ProtocolFallback::QresyncEnableFailed)
                    .await;
            }
        };
        let exact = preview.examine_uids();
        let opened = if let Some(known) = exact {
            enabled
                .examine_known(
                    &self.lease.key,
                    &self.mailbox,
                    preview.uid_validity,
                    preview.base_modseq,
                    known,
                )
                .await
        } else {
            enabled.examine_plain(&self.lease.key, &self.mailbox).await
        };
        let mut opened = match opened {
            Ok(opened) => opened,
            Err(_) => {
                return self
                    .condstore_fallback(fence, floors, ProtocolFallback::QresyncFailed)
                    .await;
            }
        };
        let selected = opened.selected.clone();
        self.observe_selected(&selected)?;
        let mut evidence = std::mem::take(&mut opened.evidence);
        let mut deletion = self.fence_qresync_evidence(&selected, &evidence)?;
        let epoch_floors = floors.for_epoch(selected.uid_validity)?;
        if let Some((reason, level)) = invalidation(&epoch_floors, &evidence) {
            self.fence_protocol_fallback(&selected, reason, level)?;
            drop(opened);
            return self.qresync_reconcile(reason).await;
        }
        let delta::ModSeq::Highest(highest_modseq) = opened.modseq else {
            self.fence_protocol_fallback(
                &selected,
                ProtocolFallback::Nomodseq,
                DeltaInvalidation::All,
            )?;
            drop(opened);
            return self.qresync_reconcile(ProtocolFallback::Nomodseq).await;
        };
        if let Some(level) = epoch_floors.invalidation(highest_modseq) {
            self.fence_protocol_fallback(&selected, ProtocolFallback::ModseqRollback, level)?;
            drop(opened);
            return self
                .qresync_reconcile(ProtocolFallback::ModseqRollback)
                .await;
        }
        if selected.uid_validity != preview.uid_validity {
            drop(opened);
            return self
                .qresync_reconcile(ProtocolFallback::QresyncFailed)
                .await;
        }
        let observation = DeltaObservation {
            uid_validity: selected.uid_validity,
            uid_next: selected.uid_next,
            highest_modseq,
            checked_at_ms: now()?,
        };
        let exact = exact.is_some();
        let start_fence = fence.clone();
        let start = self
            .local(move |scope| {
                scope.observe_delta(observation)?;
                let started =
                    scope
                        .cache
                        .start_qresync(&preview, observation, &start_fence, &|| {
                            !scope.current(observation.uid_validity)
                        })?;
                Ok((started, preview))
            })
            .await;
        let (started, preview) = match start {
            Ok(start) => start,
            Err(_) if deletion => {
                drop(opened);
                return self
                    .qresync_reconcile(ProtocolFallback::QresyncReconciliationRequired)
                    .await;
            }
            Err(error) => return Err(error),
        };
        self.report_protocol(ActiveProtocol::Qresync, None);
        let round = match started {
            DeltaStart::NeedsBaseline => {
                drop(opened);
                return self
                    .qresync_reconcile(ProtocolFallback::QresyncFailed)
                    .await;
            }
            DeltaStart::Rollback => {
                drop(opened);
                return self
                    .qresync_reconcile(ProtocolFallback::ModseqRollback)
                    .await;
            }
            DeltaStart::Current => {
                drop(opened);
                let result = self
                    .local(move |scope| {
                        let flags = flags(&evidence);
                        let receipt = DeltaObservation {
                            checked_at_ms: now()?,
                            ..observation
                        };
                        scope.cache.commit_qresync_current(
                            &preview,
                            receipt,
                            changes(&evidence, &flags),
                            &|| !scope.current(receipt.uid_validity),
                        )?;
                        scope.finish_qresync_page(receipt.uid_validity, deletion);
                        Ok(Turn::Idle)
                    })
                    .await;
                return self.finish_qresync_turn(result, deletion).await;
            }
            DeltaStart::Round(round) => round,
        };
        let page = self
            .local(move |scope| {
                let page = scope.cache.delta_page(&round, PAGE)?;
                Ok((round, page))
            })
            .await;
        let (round, page) = match page {
            Ok(page) => page,
            Err(_) if deletion => {
                drop(opened);
                return self
                    .qresync_reconcile(ProtocolFallback::QresyncReconciliationRequired)
                    .await;
            }
            Err(error) => return Err(error),
        };
        if exact && (!page.complete || page.uids != preview.known) {
            drop(opened);
            return self
                .qresync_reconcile(ProtocolFallback::QresyncFailed)
                .await;
        }
        if !exact && !page.uids.is_empty() {
            let fetched = match Box::pin(opened.collect(&page.uids, round.base_modseq)).await {
                Ok(fetched) => fetched,
                Err(_) => {
                    drop(opened);
                    if deletion {
                        return self
                            .qresync_reconcile(ProtocolFallback::QresyncReconciliationRequired)
                            .await;
                    }
                    return self
                        .condstore_fallback(fence, floors, ProtocolFallback::QresyncFailed)
                        .await;
                }
            };
            deletion |= self.fence_qresync_evidence(&selected, &fetched)?;
            evidence = combine(evidence, fetched);
        }
        drop(opened);
        if let Some((reason, level)) = invalidation(&epoch_floors, &evidence) {
            self.fence_protocol_fallback(&selected, reason, level)?;
            return self.qresync_reconcile(reason).await;
        }
        let highest_modseq = evidence.highest_modseq.unwrap_or(highest_modseq);
        if highest_modseq < round.target_modseq {
            if deletion {
                return self
                    .qresync_reconcile(ProtocolFallback::QresyncReconciliationRequired)
                    .await;
            }
            return self
                .condstore_fallback(fence, floors, ProtocolFallback::QresyncFailed)
                .await;
        }
        let receipt = DeltaObservation {
            highest_modseq,
            checked_at_ms: now()?,
            ..observation
        };
        let result = self
            .local(move |scope| {
                let flags = flags(&evidence);
                let next = scope.cache.commit_qresync_page(
                    &round,
                    &page,
                    receipt,
                    changes(&evidence, &flags),
                    &|| !scope.current(receipt.uid_validity),
                )?;
                scope.finish_qresync_page(receipt.uid_validity, deletion);
                Ok(if next.is_some() {
                    Turn::More
                } else {
                    Turn::Idle
                })
            })
            .await;
        self.finish_qresync_turn(result, deletion).await
    }

    fn fence_qresync_evidence(
        &self,
        selected: &scan::Selected,
        evidence: &qresync::Evidence,
    ) -> Result<bool> {
        if evidence.live.is_empty() && evidence.earlier.is_empty() {
            return Ok(false);
        }
        if !self.service.persistent.fence_qresync_deletion(
            &self.lease.key,
            &self.mailbox,
            selected.uid_validity,
        ) {
            return Err(Error::Deferred);
        }
        Ok(true)
    }

    async fn finish_qresync_turn(&self, result: Result<Turn>, deletion: bool) -> Result<Turn> {
        if result.is_err() && deletion {
            self.qresync_reconcile(ProtocolFallback::QresyncReconciliationRequired)
                .await
        } else {
            result
        }
    }

    async fn qresync_reconcile(&self, reason: ProtocolFallback) -> Result<Turn> {
        self.report_protocol(ActiveProtocol::Rev1, Some(reason));
        let fence = self.reconciliation_fence().await?;
        let (session, selected) = self.connect().await?;
        self.reconcile_selected(session, selected, fence, None)
            .await
    }
}

impl Scope {
    fn finish_qresync_page(&self, validity: u32, deletion: bool) {
        if deletion {
            self.persistent
                .finish_qresync_recovery(&self.account, &self.mailbox, validity, false);
        }
    }
}

fn invalidation(
    floors: &EpochFloors,
    evidence: &qresync::Evidence,
) -> Option<(ProtocolFallback, DeltaInvalidation)> {
    if evidence.modseq_unavailable {
        Some((ProtocolFallback::Nomodseq, DeltaInvalidation::All))
    } else {
        evidence
            .lowest_modseq
            .and_then(|lowest| floors.invalidation(lowest))
            .map(|level| (ProtocolFallback::ModseqRollback, level))
    }
}

fn combine(mut opened: qresync::Evidence, fetched: qresync::Evidence) -> qresync::Evidence {
    opened.flags = fetched.flags;
    opened.earlier = fetched.earlier;
    opened.live.extend(fetched.live);
    opened.highest_modseq = fetched.highest_modseq.or(opened.highest_modseq);
    opened.lowest_modseq = opened
        .lowest_modseq
        .into_iter()
        .chain(fetched.lowest_modseq)
        .min();
    opened.modseq_unavailable |= fetched.modseq_unavailable;
    opened
}

fn flags(evidence: &qresync::Evidence) -> Vec<DeltaFlags<'_>> {
    evidence
        .flags
        .iter()
        .map(|item| DeltaFlags {
            uid: item.uid,
            modseq: item.modseq,
            flags: &item.flags,
        })
        .collect()
}

fn changes<'a>(evidence: &'a qresync::Evidence, flags: &'a [DeltaFlags<'a>]) -> QresyncChanges<'a> {
    QresyncChanges {
        flags,
        earlier: &evidence.earlier,
        live: &evidence.live,
    }
}
