//! CONDSTORE orchestration over the existing bounded background turn.
use super::{super::delta, *};

enum Selection {
    Plain(Session, scan::Selected),
    Extended(Session, scan::Selected, delta::ModSeq),
}

pub(super) struct ScopeFloors {
    lane: Option<DeltaStatus>,
    deletion_lane: Option<DeltaStatus>,
    seed: Option<(u32, u64)>,
}

#[derive(Default)]
pub(super) struct EpochFloors {
    committed: Option<u64>,
    seed: Option<u64>,
}

impl EpochFloors {
    pub(super) fn invalidation(&self, highest: u64) -> Option<DeltaInvalidation> {
        if self.committed.is_some_and(|floor| highest < floor) {
            Some(DeltaInvalidation::All)
        } else if self.seed.is_some_and(|floor| highest < floor) {
            Some(DeltaInvalidation::Seed)
        } else {
            None
        }
    }
}

impl ScopeFloors {
    pub(super) fn read(scope: &Scope) -> Result<Self> {
        Ok(Self {
            lane: scope
                .cache
                .delta_status(&scope.account, &scope.mailbox, DeltaLane::Flags)?,
            deletion_lane: scope.cache.delta_status(
                &scope.account,
                &scope.mailbox,
                DeltaLane::FlagsAndExpunges,
            )?,
            seed: scope
                .cache
                .pending_delta_floor(&scope.account, &scope.mailbox)?,
        })
    }

    pub(super) fn for_epoch(&self, validity: u32) -> Result<EpochFloors> {
        let floor = |lane: &Option<DeltaStatus>| {
            lane.as_ref()
                .filter(|lane| lane.uid_validity == validity)
                .map(|lane| {
                    lane.committed_modseq
                        .parse::<u64>()
                        .map_err(|_| CacheError::Corrupt)
                })
                .transpose()
        };
        let committed = floor(&self.lane)?.max(floor(&self.deletion_lane)?);
        let pending = self
            .seed
            .filter(|(epoch, _)| *epoch == validity)
            .map(|(_, floor)| floor);
        Ok(EpochFloors {
            committed,
            seed: pending,
        })
    }
}

impl Batch<'_> {
    pub(super) fn delta_invalidation_pending(
        &self,
        selected: &scan::Selected,
    ) -> Option<DeltaInvalidation> {
        self.service.persistent.delta_invalidation_pending(
            &self.lease.key,
            &self.mailbox,
            selected.uid_validity,
        )
    }

    pub(super) fn fence_invalidation(
        &self,
        selected: &scan::Selected,
        invalidation: DeltaInvalidation,
    ) -> Result<()> {
        if !self.service.persistent.fence_delta_invalidation(
            &self.lease.key,
            &self.mailbox,
            selected.uid_validity,
            invalidation,
        ) {
            return Err(Error::Deferred);
        }
        Ok(())
    }

    pub(super) fn fence_protocol_fallback(
        &self,
        selected: &scan::Selected,
        reason: ProtocolFallback,
        invalidation: DeltaInvalidation,
    ) -> Result<()> {
        self.report_protocol(ActiveProtocol::Rev1, Some(reason));
        self.fence_invalidation(selected, invalidation)
    }

    pub(super) fn report_protocol(
        &self,
        protocol: ActiveProtocol,
        protocol_fallback: Option<ProtocolFallback>,
    ) {
        if let Ok(mut status) = self.negotiated.lock() {
            *status = Some(ProtocolHealth {
                protocol,
                protocol_fallback,
            });
        }
    }

    async fn select_condstore(
        &self,
        mut session: Session,
        capability: crate::error::Result<bool>,
    ) -> Result<Selection> {
        let failure = match capability {
            Ok(false) => {
                self.report_protocol(
                    ActiveProtocol::Rev1,
                    Some(ProtocolFallback::CapabilityUnavailable),
                );
                let (session, selected) = self.select_plain(session).await?;
                return Ok(Selection::Plain(session, selected));
            }
            Ok(true) => match delta::examine(&mut session, &self.lease.key, &self.mailbox).await {
                Ok((selected, modseq)) => {
                    self.observe_selected(&selected)?;
                    return Ok(Selection::Extended(session, selected, modseq));
                }
                Err(error) => error,
            },
            Err(error) => error,
        };
        drop(session);
        self.report_protocol(
            ActiveProtocol::Rev1,
            Some(ProtocolFallback::ExtensionFailed),
        );
        // This retry shares the original scheduler permit, deadline and
        // transport budget. Never continue a failed extension session.
        let (session, selected) = self.connect().await.map_err(|_| failure)?;
        Ok(Selection::Plain(session, selected))
    }

    pub(super) async fn reconcile_auto(&self) -> Result<Turn> {
        // A later full audit can safely extend the lane to the new tail.
        // Storage keeps the existing anchor for an already covered range.
        self.condstore(false).await
    }

    pub(super) async fn delta_turn(&self) -> Result<Turn> {
        let fence = self.reconciliation_fence().await?;
        let floors = self.local(|scope| ScopeFloors::read(&scope)).await?;
        let mut session = self.login().await?;
        let capabilities = delta::capabilities(&mut session).await;
        if capabilities
            .as_ref()
            .is_ok_and(|caps| caps.qresync && caps.enable)
            && floors.deletion_lane.is_some()
        {
            let preview_fence = fence.clone();
            let preview = self
                .local(move |scope| {
                    Ok(scope.cache.preview_qresync(
                        &scope.account,
                        &scope.mailbox,
                        &preview_fence,
                    )?)
                })
                .await?;
            if let Some(preview) = preview {
                return Box::pin(self.qresync_turn(session, fence, floors, preview)).await;
            }
        }
        let missing_enable = capabilities
            .as_ref()
            .is_ok_and(|caps| caps.qresync && !caps.enable);
        let selected = self
            .select_condstore(session, capabilities.map(|caps| caps.condstore))
            .await?;
        let result = self.condstore_selected(true, fence, floors, selected).await;
        if missing_enable {
            self.record_qresync_fallback(ProtocolFallback::QresyncEnableFailed);
        }
        result
    }

    async fn condstore(&self, advance: bool) -> Result<Turn> {
        let fence = self.reconciliation_fence().await?;
        let floors = self.local(|scope| ScopeFloors::read(&scope)).await?;
        let mut session = self.login().await?;
        let capabilities = delta::supports_condstore(&mut session).await;
        let selected = self.select_condstore(session, capabilities).await?;
        self.condstore_selected(advance, fence, floors, selected)
            .await
    }

    pub(super) async fn condstore_fallback(
        &self,
        fence: Arc<ReconcileFence>,
        floors: ScopeFloors,
        reason: ProtocolFallback,
    ) -> Result<Turn> {
        self.report_protocol(ActiveProtocol::Rev1, Some(reason));
        let mut session = self.login().await?;
        let capabilities = delta::supports_condstore(&mut session).await;
        let selected = self.select_condstore(session, capabilities).await?;
        let result = self.condstore_selected(true, fence, floors, selected).await;
        self.record_qresync_fallback(reason);
        result
    }

    fn record_qresync_fallback(&self, reason: ProtocolFallback) {
        if let Ok(mut negotiated) = self.negotiated.lock() {
            let status = negotiated.get_or_insert_with(ProtocolHealth::default);
            if status.protocol_fallback.is_none() {
                status.protocol_fallback = Some(reason);
            }
        }
    }

    async fn condstore_selected(
        &self,
        advance: bool,
        fence: Arc<ReconcileFence>,
        floors: ScopeFloors,
        selected: Selection,
    ) -> Result<Turn> {
        let (mut session, selected, highest_modseq) = match selected {
            Selection::Plain(session, selected) => {
                return self
                    .reconcile_selected(session, selected, fence, None)
                    .await;
            }
            Selection::Extended(session, selected, delta::ModSeq::Unavailable) => {
                self.fence_protocol_fallback(
                    &selected,
                    ProtocolFallback::Nomodseq,
                    DeltaInvalidation::All,
                )?;
                return self
                    .reconcile_selected(session, selected, fence, None)
                    .await;
            }
            Selection::Extended(session, selected, delta::ModSeq::Highest(highest)) => {
                (session, selected, highest)
            }
        };
        self.report_protocol(ActiveProtocol::Condstore, None);
        let epoch_floors = floors.for_epoch(selected.uid_validity)?;
        if let Some(invalidation) = epoch_floors.invalidation(highest_modseq) {
            self.fence_protocol_fallback(
                &selected,
                ProtocolFallback::ModseqRollback,
                invalidation,
            )?;
            return self
                .reconcile_selected(session, selected, fence, None)
                .await;
        }
        let prior = floors.lane;
        let lost_target = prior
            .as_ref()
            .filter(|prior| prior.uid_validity == selected.uid_validity)
            .and_then(|prior| prior.target_modseq.as_deref())
            .map(|target| target.parse::<u64>().map_err(|_| CacheError::Corrupt))
            .transpose()?
            .is_some_and(|target| highest_modseq < target);
        let invalidated = self.delta_invalidation_pending(&selected);
        if !advance
            || invalidated.is_some()
            || prior.is_none_or(|prior| prior.uid_validity != selected.uid_validity)
        {
            return self
                .reconcile_selected(session, selected, fence, Some(highest_modseq))
                .await;
        }

        let observation = DeltaObservation {
            uid_validity: selected.uid_validity,
            uid_next: selected.uid_next,
            highest_modseq,
            checked_at_ms: now()?,
        };
        let start_fence = fence.clone();
        let start = self
            .local(move |scope| {
                scope.observe_delta(observation)?;
                let result = scope.cache.start_delta(
                    &scope.account,
                    &scope.mailbox,
                    DeltaLane::Flags,
                    observation,
                    &start_fence,
                    &|| !scope.current(observation.uid_validity),
                )?;
                Ok(result)
            })
            .await?;
        let round = match start {
            DeltaStart::NeedsBaseline => {
                if lost_target {
                    self.report_protocol(
                        ActiveProtocol::Rev1,
                        Some(ProtocolFallback::ModseqRollback),
                    );
                }
                return self
                    .reconcile_selected(
                        session,
                        selected,
                        fence,
                        (!lost_target).then_some(highest_modseq),
                    )
                    .await;
            }
            DeltaStart::Current => return Ok(Turn::Idle),
            DeltaStart::Round(round) => round,
            DeltaStart::Rollback => {
                self.report_protocol(ActiveProtocol::Rev1, Some(ProtocolFallback::ModseqRollback));
                return self
                    .reconcile_selected(session, selected, fence, None)
                    .await;
            }
        };
        let (round, page) = self
            .local(move |scope| {
                let page = scope.cache.delta_page(&round, PAGE)?;
                Ok((round, page))
            })
            .await?;
        let changed = if page.uids.is_empty() {
            delta::Changed::default()
        } else {
            match Box::pin(delta::collect(
                &mut session,
                &selected,
                &page.uids,
                round.base_modseq,
            ))
            .await
            {
                Ok(changed) => changed,
                Err(_) => {
                    self.report_protocol(
                        ActiveProtocol::Rev1,
                        Some(ProtocolFallback::ExtensionFailed),
                    );
                    return self.reconcile_fresh(session, fence).await;
                }
            }
        };
        let invalidation = if changed.modseq_unavailable {
            Some((ProtocolFallback::Nomodseq, DeltaInvalidation::All))
        } else {
            changed
                .lowest_modseq
                .and_then(|lowest| epoch_floors.invalidation(lowest))
                .map(|invalidation| (ProtocolFallback::ModseqRollback, invalidation))
        };
        if let Some((reason, invalidation)) = invalidation {
            self.fence_protocol_fallback(&selected, reason, invalidation)?;
            return self.reconcile_fresh(session, fence).await;
        }
        if changed
            .highest_modseq
            .is_some_and(|highest| highest < round.target_modseq)
        {
            self.report_protocol(
                ActiveProtocol::Rev1,
                Some(ProtocolFallback::ExtensionFailed),
            );
            return self.reconcile_fresh(session, fence).await;
        }
        drop(session);
        let receipt = DeltaObservation {
            highest_modseq: changed.highest_modseq.unwrap_or(highest_modseq),
            checked_at_ms: now()?,
            ..observation
        };
        self.local(move |scope| {
            let flags = changed
                .flags
                .iter()
                .map(|item| DeltaFlags {
                    uid: item.uid,
                    modseq: item.modseq,
                    flags: &item.flags,
                })
                .collect::<Vec<_>>();
            let next = scope
                .cache
                .commit_delta_page(&round, &page, receipt, &flags, &|| {
                    !scope.current(receipt.uid_validity)
                })?;
            Ok(if next.is_some() {
                Turn::More
            } else {
                Turn::Idle
            })
        })
        .await
    }

    pub(super) async fn reconcile_fresh(
        &self,
        previous: Session,
        fence: Arc<ReconcileFence>,
    ) -> Result<Turn> {
        drop(previous);
        let (session, selected) = self.connect().await?;
        self.reconcile_selected(session, selected, fence, None)
            .await
    }
}
