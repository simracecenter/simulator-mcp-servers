// SPDX-License-Identifier: GPL-3.0-or-later

// SPDX-License-Identifier: GPL-3.0-or-later

#[cfg(target_os = "windows")]
pub fn run_pipeline(
    cfg: &crate::config::PublisherConfig,
    running: std::sync::Arc<std::sync::atomic::AtomicBool>,
    status: std::sync::Arc<std::sync::Mutex<crate::publisher_status::PublisherStatus>>,
    controls_rx: std::sync::mpsc::Receiver<crate::controls::ControlRequest>,
) -> Result<(), String> {
    use std::sync::atomic::Ordering;

    /// A session clock that goes backwards by more than this inside one
    /// sub-session is a restart, not sampling jitter.
    const SESSION_CLOCK_ROLLBACK_S: f32 = 5.0;

    use crate::{
        config::Destination,
        controls::{now_wall_clock_ms, simulated_request, ControlRequest},
        delivery::{DeliveryService, DEFAULT_MAX_OUTBOX_BATCHES},
        engine::NarrativeEngine,
        lifecycle::{HeartbeatScheduler, IntervalScheduler, LifecyclePublisher},
        log_info, log_warn,
        publisher_event::{build_event, PublisherEvent},
        publisher_status::PublisherStatus,
        race_event::{EventScope, RaceEvent},
        session_info::{
            is_ai_session, parse_sub_session_id, synthetic_sub_session_id, RosterCache,
            SessionMetadata,
        },
        session_lifecycle::SessionLifecycleTracker,
        telemetry_frame::TelemetryFrame,
        transport::PublisherTransport,
    };

    /// Copy worker-owned delivery counters into the shared status surface.
    /// The worker thread is the single writer for these fields; pipeline code
    /// only ever reads them.
    fn copy_delivery_stats(s: &mut PublisherStatus, d: &DeliveryService) {
        let st = d.stats();
        s.queued_events = st.queued_events;
        s.outbox_pending_batches = st.outbox_pending_batches;
        s.events_delivered_total = st.events_delivered_total;
        s.events_rejected_total = st.events_rejected_total;
        s.events_duplicate_total = st.events_duplicate_total;
        s.events_lost_total = st.events_lost_total;
        s.calls_total = st.calls_total;
        s.calls_failed = st.calls_failed;
        s.rc_connected = st.connected;
        s.rc_last_http_status = st.last_http_status;
        s.last_post_at = st.last_post_at;
        s.last_error_kind = st.last_error_kind;
        s.token_expires_at = st.token_expires_at;
    }

    let dry_run = false;
    let simulate = None;

    let rig_id = std::env::var("COMPUTERNAME")
        .map(|n| format!("rig-{}", n.to_lowercase()))
        .unwrap_or_else(|_| "rig-unknown".to_string());

    // ── Initialise transport + auth warmup ───────────────────────────────

    let mut transport = match cfg.destination {
        Destination::Local => {
            let local = cfg.local.as_ref().expect("validated: local config present");
            match PublisherTransport::new_local(
                &local.url,
                &local.token,
                local.cert_fingerprint.as_deref(),
                cfg.publisher.batch_interval_ms,
            ) {
                Ok(t) => t,
                Err(e) => {
                    log_warn!("[publisher] local transport setup failed: {e}");
                    return Err(format!("local transport setup failed: {e}"));
                }
            }
        }
        Destination::RaceControl => {
            let auth = cfg.auth.as_ref().expect("validated: auth config present");
            PublisherTransport::new(
                &auth.tenant_id,
                &auth.client_id,
                &auth.client_secret,
                &auth.scope,
                &cfg.publisher.rc_api_url,
                cfg.publisher.batch_interval_ms,
            )
        }
    };
    transport.set_dry_run(dry_run);
    if dry_run {
        log_info!("[publisher] dry-run mode — payloads printed to stdout; nothing is sent");
    }
    // Warm up auth early so misconfiguration is visible before the first
    // event-triggered ingest attempt. Non-fatal: posting still retries and
    // refreshes tokens if this startup token expires before first publish.
    match transport.warmup_auth() {
        Ok(()) => {
            log_info!("[publisher] auth warmup succeeded");
            let mut s = status.lock().unwrap();
            s.rc_connected = true;
            s.token_expires_at = transport.token_expires_at();
        }
        Err(e) => {
            log_warn!("[publisher] auth warmup failed: {e}");
            let mut s = status.lock().unwrap();
            s.rc_connected = false;
            s.token_expires_at = None;
            s.last_error_kind = Some(e.kind.label());
        }
    }

    // ── Delivery worker ───────────────────────────────────────────────────
    // All HTTP, token refresh and retry sleeps live on this thread — the
    // sampling loop below only pushes into a bounded in-memory queue. Each
    // batch is persisted to the on-disk outbox before posting and deleted
    // only after a 2xx, so a kill between send and ack re-delivers on the
    // next launch and a dead receiver never stalls the frame loop.
    let outbox_dir = crate::headless::data_dir().join("outbox");
    let delivery = match DeliveryService::start(transport, &outbox_dir, DEFAULT_MAX_OUTBOX_BATCHES)
    {
        Ok(d) => d,
        Err(e) => {
            log_warn!("[publisher] outbox setup failed: {e}");
            return Err(format!("outbox setup failed: {e}"));
        }
    };

    // ── Wait for iRacing ──────────────────────────────────────────────────

    log_info!("[publisher] waiting for iRacing...");
    let mut reader = match connect_loop(&running) {
        Some(r) => r,
        None => return Ok(()),
    };

    // ── Initialise components ─────────────────────────────────────────────

    let mut engine = NarrativeEngine::new(10);
    let mut lifecycle = LifecyclePublisher::new(env!("CARGO_PKG_VERSION"));
    let mut heartbeat = HeartbeatScheduler::new(cfg.publisher.heartbeat_interval_ms);
    let mut driver_material = IntervalScheduler::new(cfg.publisher.driver_material_interval_ms);
    let mut roster_cache = RosterCache::new();
    let mut session_lifecycle = SessionLifecycleTracker::new();
    let mut race_session_id = String::from("0");
    let mut sub_session_id: i64 = 0;
    let mut last_session_num: Option<i32> = None;
    // `(sub_session_id, session_num, session_time)` of the last frame seen, for
    // detecting a session clock restart inside one sub-session.
    let mut last_session_clock: Option<(i64, i32, f32)> = None;
    let mut current_session_meta: Option<SessionMetadata> = None;
    let mut last_frame: Option<TelemetryFrame> = None;
    let mut session_info_read_failures: u32 = 0;
    let mut sub_session_blocked_frames: u32 = 0;
    let mut emit_iracing_connected = true;
    // Car-scoped events held back while the roster hasn't yet resolved driverName.
    // Flushed after each roster update. Capped at 64 entries.
    let mut pending_events: Vec<PublisherEvent> = Vec::new();

    // ── Driver controls ───────────────────────────────────────────────────
    // Wheel buttons are read on their own thread — started by the caller
    // before this pipeline waits for iRacing — and handed over as accepted
    // requests; requests arriving before the roster resolves the driver are
    // held here so the published event always carries a real driver identity.
    let mut pending_requests: Vec<ControlRequest> = simulate
        .map(|action| vec![simulated_request(action, now_wall_clock_ms())])
        .unwrap_or_default();
    if let Some(action) = simulate {
        log_info!("[controls] --simulate {action}: request queued");
    }

    // ── First frame ───────────────────────────────────────────────────────
    // PUBLISHER_HELLO is deferred: it is emitted once the SessionInfo YAML is
    // parsed (so subSessionId and driverName are both resolved before the first
    // batch is sent). The `lifecycle.is_fresh()` path in the main loop handles it.

    if let Some(frame) = reader.read_frame() {
        let car_info = roster_cache
            .roster()
            .and_then(|r| r.lookup(frame.player_car_idx))
            .map(|c| format!("car=#{} {}", c.car_number, c.driver_name))
            .unwrap_or_else(|| format!("carIdx={}", frame.player_car_idx));
        log_info!("[publisher] connected — {car_info}");

        {
            let mut s = status.lock().unwrap();
            s.iracing_connected = true;
            s.current_lap = frame.lap;
            s.session_tick = frame.session_tick;
            s.session_time_secs = frame.session_time as f64;
        }

        last_frame = Some(frame);
    }

    log_info!(
        "[publisher] publishing at 60 Hz (batch every {}ms)",
        cfg.publisher.batch_interval_ms
    );

    // ── Main loop ─────────────────────────────────────────────────────────

    while running.load(Ordering::SeqCst) {
        if !reader.is_connected() {
            log_info!("[publisher] iRacing disconnected — reconnecting...");
            if sub_session_id > 0 {
                if let Some(frame) = &last_frame {
                    let disconnected = RaceEvent::IracingDisconnected {
                        lap: frame.lap,
                        session_time: frame.session_time,
                    };
                    log_event(&disconnected, roster_cache.roster(), frame);
                    let pe = build_event(
                        &disconnected,
                        frame,
                        roster_cache.roster(),
                        &race_session_id,
                        &rig_id,
                        current_session_meta.as_ref(),
                        Some(sub_session_id),
                    );
                    delivery.enqueue(pe);
                    status.lock().unwrap().events_enqueued_total += 1;
                    delivery.request_flush(
                        frame.session_time as f64,
                        frame.session_tick,
                        sub_session_id,
                    );
                }
            }
            {
                let mut s = status.lock().unwrap();
                s.iracing_connected = false;
                s.track_name = None;
                s.session_type = None;
                s.session_laps = None;
            }
            drop(reader);
            reader = match connect_loop(&running) {
                Some(r) => r,
                None => break,
            };
            engine = NarrativeEngine::new(10);
            lifecycle = LifecyclePublisher::new(env!("CARGO_PKG_VERSION"));
            roster_cache = RosterCache::new();
            session_lifecycle = SessionLifecycleTracker::new();
            pending_events.clear();
            sub_session_id = 0;
            last_session_num = None;
            last_session_clock = None;
            current_session_meta = None;
            race_session_id = String::new();
            emit_iracing_connected = true;
            log_info!("[publisher] reconnected");
            status.lock().unwrap().iracing_connected = true;
            continue;
        }

        match reader.wait_for_frame() {
            Ok(true) => {
                let Some(frame) = reader.read_frame() else {
                    continue;
                };
                let session_num_changed = last_session_num != Some(frame.session_num);
                // A session clock running backwards inside what still looks
                // like the same sub-session usually means iRacing has moved on
                // and the SessionInfo carrying the new SubSessionID has not been
                // parsed yet. Re-read it now so nothing below is stamped with
                // the departed id.
                let clock_rolled_back =
                    last_session_clock.is_some_and(|(prev_sid, prev_num, prev_t)| {
                        prev_sid == sub_session_id
                            && prev_num == frame.session_num
                            && frame.session_time + SESSION_CLOCK_ROLLBACK_S < prev_t
                    });

                // Refresh roster + session metadata when SessionInfo changes.
                // Also force a refresh when SessionNum changes so practice/qualify/race
                // transitions are not missed if the SessionInfoUpdate var is absent.
                if session_num_changed
                    || clock_rolled_back
                    || roster_cache.needs_update(frame.session_info_update)
                {
                    if let Some(yaml) = reader.read_session_info() {
                        session_info_read_failures = 0;
                        // For AI/offline sessions iRacing never assigns a real
                        // SubSessionID (always 0). Detect this and synthesise a
                        // stable negative ID so events are still published.
                        let new_sid = parse_sub_session_id(&yaml).or_else(|| {
                            if is_ai_session(&yaml) {
                                let sid = synthetic_sub_session_id(&yaml);
                                log_info!("[publisher] AI session detected — using synthetic subSessionId {sid}");
                                Some(sid)
                            } else {
                                None
                            }
                        });

                        // ── Session transition (practice→qualify→race etc.) ────────
                        // When SubSessionID changes to a new non-zero value the iRacing
                        // session has advanced. Reset all session-scoped state so stale
                        // engine signals, pending events, and lifecycle from the old
                        // session don't bleed into the new one.
                        if let Some(sid) = new_sid {
                            if sid != sub_session_id {
                                if sub_session_id > 0 {
                                    // Flush whatever was in flight for the old session.
                                    log_info!(
                                        "[publisher] session transition {sub_session_id} → {sid} — resetting engine"
                                    );
                                    // Stamped with the *live* sub-session: a
                                    // consumer already tracking `sid` drops
                                    // anything carrying the departed id, and
                                    // this is the one moment it must not.
                                    let goodbye = lifecycle.on_session_transition(
                                        frame.lap,
                                        frame.session_time,
                                        sub_session_id,
                                    );
                                    let pe = build_event(
                                        &goodbye,
                                        &frame,
                                        None,
                                        &sid.to_string(),
                                        &rig_id,
                                        None,
                                        Some(sid),
                                    );
                                    delivery.enqueue(pe);
                                    status.lock().unwrap().events_enqueued_total += 1;
                                    delivery.request_flush(
                                        frame.session_time as f64,
                                        frame.session_tick,
                                        sid,
                                    );
                                }
                                // Reset session-scoped state.
                                let previous_sub_session_id = sub_session_id;
                                engine = NarrativeEngine::new(10);
                                lifecycle = LifecyclePublisher::new(env!("CARGO_PKG_VERSION"));
                                roster_cache = RosterCache::new();
                                session_lifecycle = SessionLifecycleTracker::new();
                                pending_events.clear();
                                driver_material = IntervalScheduler::new(
                                    cfg.publisher.driver_material_interval_ms,
                                );

                                // Tell the consumer explicitly that every car
                                // index it cached belongs to the old session.
                                // Published against the *new* session id, since
                                // that is the state the consumer must adopt.
                                if previous_sub_session_id > 0 {
                                    let reset = RaceEvent::SessionReset {
                                        lap: frame.lap,
                                        session_time: frame.session_time,
                                        previous_sub_session_id: Some(previous_sub_session_id),
                                        sub_session_id: sid,
                                        previous_session_num: last_session_num,
                                        session_num: frame.session_num,
                                        previous_session_time: None,
                                        reason: "sub_session_changed".to_owned(),
                                    };
                                    let pe = build_event(
                                        &reset,
                                        &frame,
                                        None,
                                        &sid.to_string(),
                                        &rig_id,
                                        None,
                                        Some(sid),
                                    );
                                    delivery.enqueue(pe);
                                    status.lock().unwrap().events_enqueued_total += 1;
                                }
                            }
                            sub_session_id = sid;
                            race_session_id = sid.to_string();
                        }

                        roster_cache.update(frame.session_info_update, &yaml).ok();

                        // Flush car-scoped events buffered before driverName and
                        // subSessionId were both resolved. Both must be ready before
                        // any event is handed to the transport — a subSessionId of 0
                        // would persist events against a ghost session in Cosmos DB.
                        if sub_session_id > 0 && !pending_events.is_empty() {
                            let held = std::mem::take(&mut pending_events);
                            for mut pe in held {
                                if pe.scope == EventScope::CarScoped {
                                    if let Some(car_ref) = pe.car.as_mut() {
                                        if let Some(car) = roster_cache
                                            .roster()
                                            .and_then(|r| r.lookup(car_ref.car_idx))
                                        {
                                            if !car.driver_name.is_empty() {
                                                *car_ref = car.clone();
                                                delivery.enqueue(pe);
                                                status.lock().unwrap().events_enqueued_total += 1;
                                                continue;
                                            }
                                        }
                                    }
                                    pending_events.push(pe); // still unresolved
                                    continue;
                                }

                                delivery.enqueue(pe);
                                status.lock().unwrap().events_enqueued_total += 1;
                            }
                        }

                        let session_meta = SessionMetadata::parse(&yaml, frame.session_num);
                        if let Some(track_length_m) = session_meta.track_length_m {
                            engine.set_track_length_m(track_length_m);
                        }
                        current_session_meta = Some(session_meta.clone());
                        {
                            let mut s = status.lock().unwrap();
                            s.sub_session_id = Some(sub_session_id);
                            s.track_name = session_meta.track_name.clone();
                            s.session_type = session_meta.session_type.clone();
                            s.session_laps = session_meta.session_laps.clone();
                        }

                        // Emit HELLO for this session if lifecycle was just reset
                        // (first parse or session transition). Guard on sub_session_id > 0
                        // so the envelope is never posted with subSessionId=0 — the
                        // tick_result guard would block the batch anyway, but building
                        // and queueing a HELLO with race_session_id="0" could leave
                        // a stale event in the transport queue after the first transition.
                        if lifecycle.is_fresh() && sub_session_id > 0 {
                            if emit_iracing_connected {
                                let connected = RaceEvent::IracingConnected {
                                    lap: frame.lap,
                                    session_time: frame.session_time,
                                };
                                log_event(&connected, roster_cache.roster(), &frame);
                                let pe = build_event(
                                    &connected,
                                    &frame,
                                    roster_cache.roster(),
                                    &race_session_id,
                                    &rig_id,
                                    current_session_meta.as_ref(),
                                    Some(sub_session_id),
                                );
                                delivery.enqueue(pe);
                                status.lock().unwrap().events_enqueued_total += 1;
                                emit_iracing_connected = false;
                            }

                            let hello = lifecycle.on_activate(frame.lap, frame.session_time);
                            let pe = build_event(
                                &hello,
                                &frame,
                                roster_cache.roster(),
                                &race_session_id,
                                &rig_id,
                                current_session_meta.as_ref(),
                                (sub_session_id > 0).then_some(sub_session_id),
                            );
                            if pe.scope == EventScope::CarScoped
                                && pe
                                    .car
                                    .as_ref()
                                    .is_some_and(|car| car.driver_name.is_empty())
                            {
                                pending_events.push(pe);
                            } else {
                                delivery.enqueue(pe);
                                status.lock().unwrap().events_enqueued_total += 1;
                            }
                        }

                        last_session_num = Some(frame.session_num);
                    } else {
                        session_info_read_failures = session_info_read_failures.saturating_add(1);
                        if session_info_read_failures == 1 || session_info_read_failures % 300 == 0
                        {
                            log_warn!(
                                "[publisher] SessionInfo read failed (update={}, session_num={}, attempts={})",
                                frame.session_info_update,
                                frame.session_num,
                                session_info_read_failures,
                            );
                        }
                    }
                }

                // The session clock restarting inside one sub-session is a
                // session reset iRacing does not otherwise announce: same
                // subSessionId, same sessionNum, clock back near zero. Say it
                // explicitly rather than leaving the consumer to infer it from
                // tick-versus-time.
                if let Some((prev_sid, prev_num, prev_t)) = last_session_clock {
                    if prev_sid == sub_session_id
                        && prev_num == frame.session_num
                        && frame.session_time + SESSION_CLOCK_ROLLBACK_S < prev_t
                    {
                        let reset = RaceEvent::SessionReset {
                            lap: frame.lap,
                            session_time: frame.session_time,
                            previous_sub_session_id: Some(sub_session_id),
                            sub_session_id,
                            previous_session_num: Some(prev_num),
                            session_num: frame.session_num,
                            previous_session_time: Some(prev_t),
                            reason: "session_clock_restarted".to_owned(),
                        };
                        log_info!(
                            "[publisher] SESSION_RESET \u{2014} session clock restarted {prev_t:.2}s -> {:.2}s",
                            frame.session_time,
                        );
                        let pe = build_event(
                            &reset,
                            &frame,
                            roster_cache.roster(),
                            &race_session_id,
                            &rig_id,
                            current_session_meta.as_ref(),
                            Some(sub_session_id),
                        );
                        delivery.enqueue(pe);
                        status.lock().unwrap().events_enqueued_total += 1;
                    }
                }
                last_session_clock = (sub_session_id > 0).then_some((
                    sub_session_id,
                    frame.session_num,
                    frame.session_time,
                ));

                // Session type / phase / imminent-change signal. Runs after the
                // SessionInfo refresh so the type and sub-session are current.
                if sub_session_id > 0 {
                    let session_type = current_session_meta
                        .as_ref()
                        .and_then(|m| m.session_type.as_deref());
                    if let Some(event) = session_lifecycle.update(&frame, session_type) {
                        log_event(&event, roster_cache.roster(), &frame);
                        let log_entry = make_log_entry(&event, &frame, roster_cache.roster());
                        let pe = build_event(
                            &event,
                            &frame,
                            roster_cache.roster(),
                            &race_session_id,
                            &rig_id,
                            current_session_meta.as_ref(),
                            Some(sub_session_id),
                        );
                        delivery.enqueue(pe);
                        let mut s = status.lock().unwrap();
                        s.events_enqueued_total += 1;
                        s.push_event_log(log_entry);
                    }
                }

                let roster = roster_cache.roster();
                let events = engine.process_frame(&frame);

                for event in &events {
                    log_event(event, roster, &frame);
                    let log_entry = make_log_entry(event, &frame, roster);
                    let pe = build_event(
                        event,
                        &frame,
                        roster,
                        &race_session_id,
                        &rig_id,
                        current_session_meta.as_ref(),
                        (sub_session_id > 0).then_some(sub_session_id),
                    );
                    // Gate on roster: hold back events whose driverName is not yet resolved
                    // (server rejects car-scoped events with an empty driverName).
                    if pe.scope == EventScope::CarScoped
                        && pe
                            .car
                            .as_ref()
                            .is_some_and(|car| car.driver_name.is_empty())
                    {
                        status.lock().unwrap().push_event_log(log_entry);
                        if pending_events.len() < 64 {
                            pending_events.push(pe);
                        }
                    } else {
                        delivery.enqueue(pe);
                        let mut s = status.lock().unwrap();
                        s.events_enqueued_total += 1;
                        s.push_event_log(log_entry);
                    }
                }

                // Frame-level status — delivery counters come from the
                // worker, the single writer for those fields.
                {
                    let mut s = status.lock().unwrap();
                    s.current_lap = frame.lap;
                    s.session_tick = frame.session_tick;
                    s.session_time_secs = frame.session_time as f64;
                    copy_delivery_stats(&mut s, &delivery);
                }

                // Flush — skip until subSessionId is resolved to avoid persisting
                // events against a ghost session keyed on subSessionId=0.
                if sub_session_id == 0 {
                    sub_session_blocked_frames = sub_session_blocked_frames.saturating_add(1);
                    if sub_session_blocked_frames == 1 || sub_session_blocked_frames % 300 == 0 {
                        log_warn!(
                            "[publisher] publishing paused: unresolved subSessionId (SessionInfoUpdate={})",
                            frame.session_info_update,
                        );
                    }
                    continue;
                }
                sub_session_blocked_frames = 0;

                // Driver control requests. Published as soon as the requesting
                // driver is known and flushed immediately — the broadcast agent
                // must react to a button press, not to the next batch window.
                while let Ok(request) = controls_rx.try_recv() {
                    if pending_requests.len() < 8 {
                        pending_requests.push(request);
                    } else {
                        log_warn!("[controls] dropping request: driver identity unresolved");
                    }
                }
                if !pending_requests.is_empty() {
                    let requester = roster
                        .and_then(|r| r.lookup(frame.player_car_idx))
                        .filter(|car| !car.driver_name.is_empty());
                    if let Some(car) = requester {
                        let driver_id = car.driver_id();
                        for request in std::mem::take(&mut pending_requests) {
                            let event = control_race_event(&request, &frame, &driver_id, &rig_id);
                            log_event(&event, roster, &frame);
                            let log_entry = make_log_entry(&event, &frame, roster);
                            let pe = build_event(
                                &event,
                                &frame,
                                roster,
                                &race_session_id,
                                &rig_id,
                                current_session_meta.as_ref(),
                                Some(sub_session_id),
                            );
                            delivery.enqueue(pe);
                            {
                                let mut s = status.lock().unwrap();
                                s.events_enqueued_total += 1;
                                s.push_event_log(log_entry);
                            }
                        }
                        delivery.request_flush(
                            frame.session_time as f64,
                            frame.session_tick,
                            sub_session_id,
                        );
                    }
                }

                // Heartbeat — rig-scoped liveness signal on a wall-clock timer,
                // gated on the same connected + resolved-subSessionId conditions
                // as HELLO, delivered via the normal batch path.
                if heartbeat.due(std::time::Instant::now()) {
                    let enqueued_total = status.lock().unwrap().events_enqueued_total;
                    let hb = lifecycle.heartbeat(frame.lap, frame.session_time, enqueued_total);
                    let pe = build_event(
                        &hb,
                        &frame,
                        roster,
                        &race_session_id,
                        &rig_id,
                        current_session_meta.as_ref(),
                        Some(sub_session_id),
                    );
                    delivery.enqueue(pe);
                    status.lock().unwrap().events_enqueued_total += 1;
                }

                // Periodic driver material — the rig's own driver on a
                // wall-clock cadence, so the consumer always has something
                // current to cover even through a quiet stint.
                if let Some(event) = driver_material
                    .due_elapsed(std::time::Instant::now())
                    .and_then(|elapsed| engine.driver_material(&frame, elapsed.as_secs_f32()))
                {
                    let log_entry = make_log_entry(&event, &frame, roster);
                    let pe = build_event(
                        &event,
                        &frame,
                        roster,
                        &race_session_id,
                        &rig_id,
                        current_session_meta.as_ref(),
                        Some(sub_session_id),
                    );
                    // Nothing worth saying about a driver the roster cannot name
                    // yet; the next cadence tick carries the same state anyway,
                    // so this is dropped rather than buffered.
                    if !pe
                        .car
                        .as_ref()
                        .is_some_and(|car| car.driver_name.is_empty())
                    {
                        delivery.enqueue(pe);
                        let mut s = status.lock().unwrap();
                        s.events_enqueued_total += 1;
                        s.push_event_log(log_entry);
                    }
                }

                // Stamp the session envelope for the next batch; the worker
                // owns batch pacing, posting and retry.
                delivery.tick(
                    frame.session_time as f64,
                    frame.session_tick,
                    sub_session_id,
                );

                last_frame = Some(frame);
            }
            Ok(false) => {}
            Err(e) => log_warn!("[publisher] frame read error: {e}"),
        }
    }

    // ── Clean shutdown ────────────────────────────────────────────────────

    let (bye_lap, bye_t) = last_frame
        .as_ref()
        .map(|f| (f.lap, f.session_time))
        .unwrap_or((0, 0.0));

    let goodbye = lifecycle.on_deactivate(bye_lap, bye_t);
    if let Some(frame) = &last_frame {
        let pe = build_event(
            &goodbye,
            frame,
            roster_cache.roster(),
            &race_session_id,
            &rig_id,
            current_session_meta.as_ref(),
            (sub_session_id > 0).then_some(sub_session_id),
        );
        delivery.enqueue(pe);
    }

    log_info!("[publisher] PUBLISHER_GOODBYE sent — draining outbox...");
    delivery.shutdown(bye_t as f64, 0, sub_session_id);
    {
        let mut s = status.lock().unwrap();
        copy_delivery_stats(&mut s, &delivery);
    }
    Ok(())
}

/// Turn an accepted control request into the event published to Race Control.
///
/// `driver_id` identifies the driver at the wheel of `frame.player_car_idx`;
/// the sandbox uses it to resolve the requester's onboard scene when several
/// rigs are configured.
#[cfg(target_os = "windows")]
fn control_race_event(
    request: &crate::controls::ControlRequest,
    frame: &crate::telemetry_frame::TelemetryFrame,
    driver_id: &str,
    rig_id: &str,
) -> crate::race_event::RaceEvent {
    use crate::controls::{ControlAction, FOCUS_DWELL_MS};
    use crate::race_event::RaceEvent;

    match request.action {
        ControlAction::FocusMe => RaceEvent::FocusMeRequested {
            lap: frame.lap,
            session_time: frame.session_time,
            player_car_idx: frame.player_car_idx,
            request_id: request.request_id.clone(),
            press_seq: request.press_seq,
            driver_id: driver_id.to_owned(),
            rig_id: rig_id.to_owned(),
            source: request.source.clone(),
            button: request.button,
            requested_at_ms: request.requested_at_ms,
            dwell_ms: FOCUS_DWELL_MS,
        },
        ControlAction::BroadcastToggle => RaceEvent::BroadcastControlRequested {
            lap: frame.lap,
            session_time: frame.session_time,
            action: "toggle".to_owned(),
            request_id: request.request_id.clone(),
            press_seq: request.press_seq,
            driver_id: driver_id.to_owned(),
            rig_id: rig_id.to_owned(),
            source: request.source.clone(),
            button: request.button,
            requested_at_ms: request.requested_at_ms,
        },
    }
}

/// Retry loop: block until `SharedMemReader::try_connect()` succeeds.
/// Returns `None` if the shutdown flag is set before a connection is made.
#[cfg(target_os = "windows")]
fn connect_loop(
    running: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Option<crate::sim_bridge::SharedMemReader> {
    use crate::sim_bridge::SharedMemReader;
    use std::sync::atomic::Ordering;
    loop {
        if !running.load(Ordering::SeqCst) {
            return None;
        }
        match SharedMemReader::try_connect() {
            Ok(r) => return Some(r),
            Err(_) => std::thread::sleep(std::time::Duration::from_secs(1)),
        }
    }
}

/// Build an `EventLogEntry` for the status panel.
#[cfg(target_os = "windows")]
fn make_log_entry(
    event: &crate::race_event::RaceEvent,
    frame: &crate::telemetry_frame::TelemetryFrame,
    roster: Option<&crate::session_info::SessionRoster>,
) -> crate::publisher_status::EventLogEntry {
    use crate::publisher_status::EventLogEntry;

    let mut event_value = serde_json::to_value(event).unwrap_or_default();
    let event_type = event_value
        .as_object_mut()
        .and_then(|m| m.remove("event_type"))
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();

    let car = roster.and_then(|r| r.lookup(frame.player_car_idx));
    EventLogEntry {
        session_time: frame.session_time as f64,
        event_type,
        car_number: car.map(|c| c.car_number.clone()).unwrap_or_default(),
        driver_name: car.map(|c| c.driver_name.clone()).unwrap_or_default(),
    }
}

/// Print a concise log line for notable narrative events.
#[cfg(target_os = "windows")]
fn log_event(
    event: &crate::race_event::RaceEvent,
    roster: Option<&crate::session_info::SessionRoster>,
    _frame: &crate::telemetry_frame::TelemetryFrame,
) {
    use crate::log_info;
    use crate::race_event::RaceEvent;
    use crate::session_info::SessionRoster;

    fn car_num(roster: Option<&SessionRoster>, idx: u8) -> String {
        roster
            .and_then(|r| r.lookup(idx))
            .map(|c| c.car_number.clone())
            .unwrap_or_else(|| idx.to_string())
    }

    match event {
        RaceEvent::RaceGreen { .. } => {
            log_info!("[publisher] RACE_GREEN — session event, lap 1 underway")
        }
        RaceEvent::RaceCheckered { .. } => log_info!("[publisher] RACE_CHECKERED — session event"),
        RaceEvent::FlagYellowFullCourse { .. } => {
            log_info!("[publisher] FLAG_YELLOW_FULL_COURSE — session event")
        }
        RaceEvent::FlagYellowLocal { .. } => log_info!("[publisher] FLAG_YELLOW_LOCAL"),
        RaceEvent::IracingConnected { .. } => {
            log_info!("[publisher] IRACING_CONNECTED — telemetry feed available")
        }
        RaceEvent::IracingDisconnected { .. } => {
            log_info!("[publisher] IRACING_DISCONNECTED — telemetry feed dropped")
        }
        RaceEvent::DriverEnteredCar { player_car_idx, .. } => {
            let player = car_num(roster, *player_car_idx);
            log_info!("[publisher] DRIVER_ENTERED_CAR — #{player}");
        }
        RaceEvent::DriverExitedCar { player_car_idx, .. } => {
            let player = car_num(roster, *player_car_idx);
            log_info!("[publisher] DRIVER_EXITED_CAR — #{player}");
        }
        RaceEvent::IncidentAlert {
            car_idx,
            reason,
            speed_drop_mps,
            ..
        } => {
            let player = car_num(roster, *car_idx);
            log_info!("[publisher] INCIDENT_ALERT — #{player}, {reason}, speed drop {speed_drop_mps:.1} m/s");
        }
        RaceEvent::BattleEngaged {
            player_car_idx,
            opponent_car_idx,
            gap_s,
            ..
        } => {
            let player = car_num(roster, *player_car_idx);
            let opp = car_num(roster, *opponent_car_idx);
            log_info!("[publisher] BATTLE_ENGAGED — #{player} vs #{opp}, gap {gap_s:.1}s");
        }
        RaceEvent::BattleClosing {
            player_car_idx,
            opponent_car_idx,
            closing_rate_sec_per_lap,
            ..
        } => {
            let player = car_num(roster, *player_car_idx);
            let opp = car_num(roster, *opponent_car_idx);
            log_info!(
                "[publisher] BATTLE_CLOSING — #{player} vs #{opp}, \
                 closing {closing_rate_sec_per_lap:.1}s/lap"
            );
        }
        RaceEvent::Overtake {
            car_idx,
            overtaken_car_idx,
            position_to,
            ..
        } => {
            let player = car_num(roster, *car_idx);
            let overtaken = overtaken_car_idx
                .map(|idx| car_num(roster, idx))
                .unwrap_or_else(|| "?".to_string());
            log_info!("[publisher] OVERTAKE — #{player} passed #{overtaken} to P{position_to}");
        }
        RaceEvent::OvertakeForLead {
            car_idx,
            overtaken_car_idx,
            ..
        } => {
            let player = car_num(roster, *car_idx);
            let overtaken = overtaken_car_idx
                .map(|idx| car_num(roster, idx))
                .unwrap_or_else(|| "?".to_string());
            log_info!("[publisher] OVERTAKE_FOR_LEAD — #{player} passed #{overtaken}");
        }
        _ => {}
    }
}
