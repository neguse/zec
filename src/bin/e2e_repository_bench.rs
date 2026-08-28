mod e2e_support;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    path::Path,
    process,
    sync::{
        atomic::{AtomicU32, Ordering},
        mpsc::{self, Sender, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail, ensure};
use e2e_support::{
    ALT_F, BenchmarkReport, CTRL_G, CTRL_P, CTRL_Q, CTRL_S, DELETE, DOWN, ENTER, ESC, InputTrace,
    Invocation, MetricReport, PtySession, REPORT_SCHEMA_VERSION, TerminalBaseline,
    benchmark_oracle, binary_report, descendant_process_count, environment_report, fixture,
    oracle_hashes, parse_invocation, reset_fixed_fixture, verify_benchmark_oracle,
    verify_benchmark_report, vm_hwm_bytes_if_present, write_report,
};
use e2e_support::{
    SearchResultReport, expected_benchmark_quick_open_queries, expected_benchmark_search_rows,
};
use nix::libc;

const STARTUP_WARMUPS: usize = 2;
const STARTUP_SAMPLES: usize = 20;
const QUICK_WARMUPS: usize = 10;
const QUICK_SAMPLES: usize = 100;
const PROJECT_WARMUPS: usize = 2;
const PROJECT_SAMPLES: usize = 10;
const IN_FLIGHT_ATTEMPTS: usize = 20;
const EDIT_WARMUPS: usize = 10;
const EDIT_SAMPLES: usize = 500;
const SAVE_WARMUPS: usize = 2;
const SAVE_SAMPLES: usize = 10;

const NETLINK_CONNECTOR: i32 = 11;
const CN_IDX_PROC: u32 = 1;
const CN_VAL_PROC: u32 = 1;
const PROC_CN_MCAST_IGNORE: u32 = 2;
const PROC_CN_MCAST_LISTEN: u32 = 1;
const NLMSG_NOOP: u16 = 1;
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const NLMSG_OVERRUN: u16 = 4;
const PROC_EVENT_FORK: u32 = 1;
const NETLINK_HEADER_LEN: usize = 16;
const CONNECTOR_HEADER_LEN: usize = 20;
static NEXT_CONTROL_SEQUENCE: AtomicU32 = AtomicU32::new(1);

fn main() -> Result<()> {
    match parse_invocation(false)? {
        Invocation::Verify(path) => {
            let report = e2e_support::read_report::<BenchmarkReport>(&path)?;
            verify_benchmark_report(&report)
                .with_context(|| format!("verify {}", path.display()))?;
            println!("Alpha 1 benchmark report verified");
            Ok(())
        }
        Invocation::Run(arguments) => run(arguments),
    }
}

fn run(arguments: e2e_support::RunArguments) -> Result<()> {
    let benchmark_oracle = benchmark_oracle()?;
    verify_benchmark_oracle(&benchmark_oracle)?;
    let zec = fs::canonicalize(&arguments.zec).context("canonicalize --zec")?;
    let generated = reset_fixed_fixture()?;
    let mut resources = ResourceTracker::default();

    let startup = startup_metric(&zec, &generated.root, &mut resources)?;
    let quick_open = quick_open_metric(&zec, &generated.root, &mut resources)?;
    let (project_search, search_observation) =
        project_search_metric(&zec, &generated.root, &mut resources)?;
    let (replace_query, cancel_search, quit_in_flight_search) =
        in_flight_metrics(&zec, &generated.root, &mut resources)?;
    let (editing, input_trace) = editing_metric(
        &zec,
        &generated.root,
        &mut resources,
        &benchmark_oracle.editing.payload_suffix,
    )?;
    let save = save_metric(&zec, &generated.root, &mut resources)?;

    if resources.max_descendant_count > 0 {
        eprintln!(
            "Alpha 1 descendant diagnostics ({} observed): {:?}",
            resources.max_descendant_count, resources.descendant_commands
        );
    }

    let assertions = BTreeMap::from([
        ("startup_latency".to_owned(), startup.assertion_passed),
        ("quick_open_latency".to_owned(), quick_open.assertion_passed),
        (
            "project_search_latency".to_owned(),
            project_search.assertion_passed,
        ),
        (
            "replace_query_latency".to_owned(),
            replace_query.assertion_passed,
        ),
        (
            "cancel_search_latency".to_owned(),
            cancel_search.assertion_passed,
        ),
        (
            "quit_in_flight_latency".to_owned(),
            quit_in_flight_search.assertion_passed,
        ),
        ("editing_latency".to_owned(), editing.assertion_passed),
        ("save_latency".to_owned(), save.assertion_passed),
        (
            "project_search_hits".to_owned(),
            search_observation.total_hits == fixture::BENCH_SEARCH_HITS
                && search_observation.visible_results.len() == fixture::SEARCH_RESULT_LIMIT
                && search_observation.visible_results == expected_benchmark_search_rows(),
        ),
        (
            "vm_hwm".to_owned(),
            resources.max_vm_hwm_bytes > 0 && resources.max_vm_hwm_bytes <= 1_073_741_824,
        ),
        (
            "no_descendant_processes".to_owned(),
            resources.max_descendant_count == 0,
        ),
        (
            "input_ids".to_owned(),
            input_trace.sent_input_ids == input_trace.expected_input_ids
                && input_trace.applied_input_ids == input_trace.expected_input_ids
                && input_trace.dropped_count == 0
                && !input_trace.reordered,
        ),
    ]);
    let all_assertions_passed = assertions.values().all(|passed| *passed);

    let report = BenchmarkReport {
        schema_version: REPORT_SCHEMA_VERSION,
        contract_version: fixture::CONTRACT_VERSION,
        report_kind: "e2e_repository_benchmark".to_owned(),
        environment: environment_report()?,
        binary: binary_report(&zec)?,
        oracles: oracle_hashes(),
        startup,
        quick_open,
        project_search,
        replace_query,
        cancel_search,
        quit_in_flight_search,
        editing,
        save,
        project_search_total_hits: search_observation.total_hits,
        project_search_visible_results: search_observation.visible_results.len(),
        project_search_rows: search_observation.visible_results,
        vm_hwm_bytes: resources.max_vm_hwm_bytes,
        vm_hwm_limit_bytes: 1_073_741_824,
        descendant_process_count: resources.max_descendant_count,
        input_trace,
        assertions,
        all_assertions_passed,
    };
    write_report(&arguments.report, &report)?;
    println!(
        "Alpha 1 benchmark: startup p95={}us, quick-open p95={}us, search p95={}us, replace max={}us, cancel max={}us, quit max={}us, edit p95={}us, save max={}us; report {}",
        report.startup.p95_us,
        report.quick_open.p95_us,
        report.project_search.p95_us,
        report.replace_query.max_us,
        report.cancel_search.max_us,
        report.quit_in_flight_search.max_us,
        report.editing.p95_us,
        report.save.max_us,
        arguments.report.display()
    );
    if arguments.assert {
        verify_benchmark_report(&report)?;
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProjectSearchObservation {
    total_hits: usize,
    visible_results: Vec<SearchResultReport>,
}

struct ArmedResourceMonitor {
    events: ProcEventSocket,
}

struct ProcEventSocket {
    fd: OwnedFd,
    subscribed: bool,
}

impl ProcEventSocket {
    fn subscribe() -> Result<Self> {
        let raw_fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                NETLINK_CONNECTOR,
            )
        };
        if raw_fd < 0 {
            return Err(io::Error::last_os_error()).context("open proc-connector socket");
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        let receive_buffer: libc::c_int = 4 * 1024 * 1024;
        let configured = unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                (&receive_buffer as *const libc::c_int).cast(),
                std::mem::size_of_val(&receive_buffer) as libc::socklen_t,
            )
        };
        if configured < 0 {
            return Err(io::Error::last_os_error())
                .context("configure proc-connector receive buffer");
        }

        let mut address: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        address.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        address.nl_pid = process::id();
        address.nl_groups = CN_IDX_PROC;
        let bound = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                (&address as *const libc::sockaddr_nl).cast(),
                std::mem::size_of_val(&address) as libc::socklen_t,
            )
        };
        if bound < 0 {
            return Err(io::Error::last_os_error()).context("bind proc-connector socket");
        }

        let mut socket = Self {
            fd,
            subscribed: false,
        };
        let acknowledgement = socket.send_subscription(PROC_CN_MCAST_LISTEN)?;
        socket.subscribed = true;
        socket.wait_control_ack("LISTEN", acknowledgement)?;
        Ok(socket)
    }

    fn unsubscribe(&mut self) -> Result<()> {
        if self.subscribed {
            let _ = self.send_subscription(PROC_CN_MCAST_IGNORE)?;
            self.subscribed = false;
        }
        Ok(())
    }

    fn wait_control_ack(&self, operation: &str, expected_acknowledgement: u32) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let received = unsafe {
                libc::recv(
                    self.fd.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    0,
                )
            };
            if received < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::WouldBlock {
                    ensure!(
                        Instant::now() < deadline,
                        "proc-connector {operation} acknowledgement timed out"
                    );
                    thread::sleep(Duration::from_millis(1));
                    continue;
                }
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if error.raw_os_error() == Some(libc::ENOBUFS) {
                    bail!("proc-connector lost events before {operation} acknowledgement");
                }
                return Err(error).with_context(|| {
                    format!("receive proc-connector {operation} acknowledgement")
                });
            }
            ensure!(received > 0, "proc-connector returned EOF");
            if parse_proc_events(
                &buffer[..received as usize],
                &mut Vec::new(),
                Some(expected_acknowledgement),
            )? {
                return Ok(());
            }
            ensure!(
                Instant::now() < deadline,
                "proc-connector {operation} acknowledgement timed out"
            );
        }
    }

    fn send_subscription(&self, operation: u32) -> Result<u32> {
        let sequence = NEXT_CONTROL_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let request_acknowledgement = process::id();
        let message_len = NETLINK_HEADER_LEN + CONNECTOR_HEADER_LEN + 4;
        let mut message = Vec::with_capacity(message_len);
        push_u32(&mut message, message_len as u32);
        push_u16(&mut message, NLMSG_DONE);
        push_u16(&mut message, 1);
        push_u32(&mut message, sequence);
        push_u32(&mut message, process::id());
        push_u32(&mut message, CN_IDX_PROC);
        push_u32(&mut message, CN_VAL_PROC);
        push_u32(&mut message, sequence);
        push_u32(&mut message, request_acknowledgement);
        push_u16(&mut message, 4);
        push_u16(&mut message, 0);
        push_u32(&mut message, operation);
        ensure!(
            message.len() == message_len,
            "proc-connector message size differs"
        );

        let mut kernel: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        kernel.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        let sent = unsafe {
            libc::sendto(
                self.fd.as_raw_fd(),
                message.as_ptr().cast(),
                message.len(),
                0,
                (&kernel as *const libc::sockaddr_nl).cast(),
                std::mem::size_of_val(&kernel) as libc::socklen_t,
            )
        };
        if sent < 0 {
            return Err(io::Error::last_os_error()).context("subscribe to proc fork events");
        }
        ensure!(sent as usize == message.len(), "short proc-connector send");
        Ok(request_acknowledgement.wrapping_add(1))
    }

    fn drain_fork_edges(&self, edges: &mut Vec<(i32, i32)>) -> Result<()> {
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let received = unsafe {
                libc::recv(
                    self.fd.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    0,
                )
            };
            if received < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::WouldBlock {
                    return Ok(());
                }
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if error.raw_os_error() == Some(libc::ENOBUFS) {
                    bail!("proc-connector lost fork events (ENOBUFS)");
                }
                return Err(error).context("receive proc fork events");
            }
            ensure!(received > 0, "proc-connector returned EOF");
            let _ = parse_proc_events(&buffer[..received as usize], edges, None)?;
        }
    }
}

impl Drop for ProcEventSocket {
    fn drop(&mut self) {
        let _ = self.unsubscribe();
    }
}

fn parse_proc_events(
    datagram: &[u8],
    edges: &mut Vec<(i32, i32)>,
    expected_acknowledgement: Option<u32>,
) -> Result<bool> {
    let mut matched_acknowledgement = false;
    let mut offset = 0;
    while offset < datagram.len() {
        ensure!(
            datagram.len() - offset >= NETLINK_HEADER_LEN,
            "truncated netlink header"
        );
        let message_len = read_u32(datagram, offset)? as usize;
        let message_type = read_u16(datagram, offset + 4)?;
        ensure!(
            message_len >= NETLINK_HEADER_LEN && offset + message_len <= datagram.len(),
            "invalid netlink message length"
        );
        match message_type {
            NLMSG_NOOP => {}
            NLMSG_ERROR => {
                ensure!(
                    message_len >= NETLINK_HEADER_LEN + 4,
                    "truncated netlink error"
                );
                let code = read_i32(datagram, offset + NETLINK_HEADER_LEN)?;
                ensure!(code == 0, "proc-connector netlink error {code}");
            }
            NLMSG_OVERRUN => bail!("proc-connector reported lost fork events"),
            NLMSG_DONE => {
                let connector = offset + NETLINK_HEADER_LEN;
                ensure!(
                    message_len >= NETLINK_HEADER_LEN + CONNECTOR_HEADER_LEN,
                    "truncated connector header"
                );
                let index = read_u32(datagram, connector)?;
                let value = read_u32(datagram, connector + 4)?;
                let acknowledgement = read_u32(datagram, connector + 12)?;
                let payload_len = read_u16(datagram, connector + 16)? as usize;
                let payload = connector + CONNECTOR_HEADER_LEN;
                ensure!(
                    payload + payload_len <= offset + message_len,
                    "truncated proc event payload"
                );
                if index == CN_IDX_PROC && value == CN_VAL_PROC && payload_len >= 32 {
                    let event = read_u32(datagram, payload)?;
                    if event == 0 && expected_acknowledgement == Some(acknowledgement) {
                        let error = read_i32(datagram, payload + 16)?;
                        ensure!(error == 0, "proc-connector control error {error}");
                        matched_acknowledgement = true;
                    } else if event == PROC_EVENT_FORK {
                        let fork = payload + 16;
                        let parent_tgid = i32::try_from(read_u32(datagram, fork + 4)?)
                            .context("parent TGID does not fit pid_t")?;
                        let child_tgid = i32::try_from(read_u32(datagram, fork + 12)?)
                            .context("child TGID does not fit pid_t")?;
                        if child_tgid != parent_tgid {
                            edges.push((parent_tgid, child_tgid));
                        }
                    }
                }
            }
            other => bail!("unexpected netlink message type {other}"),
        }
        let aligned = (message_len + 3) & !3;
        if offset + aligned > datagram.len() {
            ensure!(
                offset + message_len == datagram.len(),
                "truncated netlink alignment padding"
            );
            break;
        }
        offset += aligned;
    }
    Ok(matched_acknowledgement)
}

fn observed_descendant_count(root: i32, edges: &[(i32, i32)]) -> usize {
    observed_descendant_pids(root, edges).len()
}

fn observed_descendant_pids(root: i32, edges: &[(i32, i32)]) -> BTreeSet<i32> {
    let mut descendants = BTreeSet::new();
    loop {
        let before = descendants.len();
        for &(parent, child) in edges {
            if (parent == root || descendants.contains(&parent)) && child != root {
                descendants.insert(child);
            }
        }
        if descendants.len() == before {
            return descendants;
        }
    }
}

fn record_descendant_commands(root: i32, edges: &[(i32, i32)], commands: &mut BTreeSet<String>) {
    for pid in observed_descendant_pids(root, edges) {
        let command_line = fs::read(format!("/proc/{pid}/cmdline"))
            .ok()
            .map(|bytes| {
                String::from_utf8_lossy(&bytes)
                    .replace('\0', " ")
                    .trim()
                    .to_owned()
            })
            .filter(|command| !command.is_empty());
        let command = command_line.or_else(|| {
            fs::read_to_string(format!("/proc/{pid}/comm"))
                .ok()
                .map(|command| command.trim().to_owned())
                .filter(|command| !command.is_empty())
        });
        if let Some(command) = command {
            commands.insert(command);
        }
    }
}

fn push_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_ne_bytes());
}

fn push_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_ne_bytes());
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    let value = bytes
        .get(offset..offset + 2)
        .context("truncated native-endian u16")?;
    Ok(u16::from_ne_bytes([value[0], value[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let value = bytes
        .get(offset..offset + 4)
        .context("truncated native-endian u32")?;
    Ok(u32::from_ne_bytes([value[0], value[1], value[2], value[3]]))
}

fn read_i32(bytes: &[u8], offset: usize) -> Result<i32> {
    Ok(i32::from_ne_bytes(read_u32(bytes, offset)?.to_ne_bytes()))
}

#[derive(Default)]
struct ResourceTracker {
    max_vm_hwm_bytes: u64,
    max_descendant_count: usize,
    descendant_commands: BTreeSet<String>,
}

impl ResourceTracker {
    fn finish(&mut self, monitor: ResourceMonitor) -> Result<()> {
        let observation = monitor.finish()?;
        self.max_vm_hwm_bytes = self.max_vm_hwm_bytes.max(observation.max_vm_hwm_bytes);
        self.max_descendant_count = self
            .max_descendant_count
            .max(observation.max_descendant_count);
        self.descendant_commands
            .extend(observation.descendant_commands);
        Ok(())
    }
}

#[derive(Default)]
struct ResourceObservation {
    max_vm_hwm_bytes: u64,
    max_descendant_count: usize,
    descendant_commands: BTreeSet<String>,
}

struct ResourceMonitor {
    stop: Sender<()>,
    handle: Option<JoinHandle<Result<ResourceObservation>>>,
}

impl ArmedResourceMonitor {
    fn start(self, pid: i32) -> Result<ResourceMonitor> {
        let initial_vm_hwm_bytes = vm_hwm_bytes_if_present(pid)?.unwrap_or(0);
        let initial_descendant_count = descendant_process_count(pid)?;
        let (stop, receiver) = mpsc::channel();
        let mut events = self.events;
        let handle = thread::Builder::new()
            .name(format!("alpha-1-resource-{pid}"))
            .spawn(move || {
                let mut fork_edges = Vec::new();
                let mut observation = ResourceObservation {
                    max_vm_hwm_bytes: initial_vm_hwm_bytes,
                    max_descendant_count: initial_descendant_count,
                    descendant_commands: BTreeSet::new(),
                };
                loop {
                    events.drain_fork_edges(&mut fork_edges)?;
                    record_descendant_commands(
                        pid,
                        &fork_edges,
                        &mut observation.descendant_commands,
                    );
                    observation.max_descendant_count = observation
                        .max_descendant_count
                        .max(observed_descendant_count(pid, &fork_edges));
                    let proc_path = format!("/proc/{pid}");
                    match vm_hwm_bytes_if_present(pid) {
                        Ok(Some(bytes)) => {
                            observation.max_vm_hwm_bytes = observation.max_vm_hwm_bytes.max(bytes);
                        }
                        Ok(None) => {}
                        Err(_) if !Path::new(&proc_path).exists() => {}
                        Err(error) => return Err(error),
                    }
                    match descendant_process_count(pid) {
                        Ok(count) => {
                            observation.max_descendant_count =
                                observation.max_descendant_count.max(count);
                        }
                        Err(_) if !Path::new(&proc_path).exists() => {}
                        Err(error) => return Err(error),
                    }
                    let stopping = match receiver.try_recv() {
                        Ok(()) | Err(TryRecvError::Disconnected) => true,
                        Err(TryRecvError::Empty) => false,
                    };
                    if stopping {
                        events.drain_fork_edges(&mut fork_edges)?;
                        record_descendant_commands(
                            pid,
                            &fork_edges,
                            &mut observation.descendant_commands,
                        );
                        observation.max_descendant_count = observation
                            .max_descendant_count
                            .max(observed_descendant_count(pid, &fork_edges));
                        break;
                    }
                    thread::sleep(Duration::from_millis(1));
                }
                events.unsubscribe()?;
                ensure!(
                    observation.max_vm_hwm_bytes > 0,
                    "VmHWM was never observed for zec"
                );
                Ok(observation)
            })
            .context("spawn continuous resource monitor")?;
        Ok(ResourceMonitor {
            stop,
            handle: Some(handle),
        })
    }
}

impl ResourceMonitor {
    fn arm() -> Result<ArmedResourceMonitor> {
        Ok(ArmedResourceMonitor {
            events: ProcEventSocket::subscribe()?,
        })
    }

    fn finish(mut self) -> Result<ResourceObservation> {
        let _ = self.stop.send(());
        let handle = self
            .handle
            .take()
            .context("resource monitor handle is absent")?;
        join_resource_monitor(handle)
    }
}

impl Drop for ResourceMonitor {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(handle) = self.handle.take() {
            let deadline = std::time::Instant::now() + e2e_support::CHILD_TIMEOUT;
            while !handle.is_finished() && std::time::Instant::now() < deadline {
                thread::sleep(Duration::from_millis(1));
            }
            if handle.is_finished() {
                let _ = handle.join();
            }
        }
    }
}

fn join_resource_monitor(
    handle: JoinHandle<Result<ResourceObservation>>,
) -> Result<ResourceObservation> {
    let deadline = std::time::Instant::now() + e2e_support::CHILD_TIMEOUT;
    while !handle.is_finished() {
        let now = std::time::Instant::now();
        ensure!(
            now < deadline,
            "resource monitor did not finish within 5 seconds"
        );
        thread::sleep((deadline - now).min(Duration::from_millis(1)));
    }
    handle
        .join()
        .map_err(|_| anyhow::anyhow!("resource monitor thread panicked"))?
}

fn spawn_ready(
    zec: &Path,
    root: &Path,
    config_name: &str,
) -> Result<(PtySession, TerminalBaseline, u64, ResourceMonitor)> {
    let config = e2e_support::fresh_config_dir(config_name)?;
    let armed_monitor = ResourceMonitor::arm()?;
    let (mut session, baseline) = PtySession::spawn(zec, root, &[root.as_os_str()], &config)?;
    let monitor = armed_monitor.start(session.pid()?)?;
    let startup_us = session.wait_ready("repo", fixture::READY_SENTINEL)?;
    session.assert_raw(&baseline)?;
    Ok((session, baseline, startup_us, monitor))
}

fn startup_metric(
    zec: &Path,
    root: &Path,
    resources: &mut ResourceTracker,
) -> Result<MetricReport> {
    let mut samples = Vec::with_capacity(STARTUP_SAMPLES);
    for index in 0..STARTUP_WARMUPS + STARTUP_SAMPLES {
        let (mut session, baseline, elapsed, monitor) =
            spawn_ready(zec, root, &format!("bench-startup-{index:02}"))?;
        if index >= STARTUP_WARMUPS {
            samples.push(elapsed);
        }
        quit_clean(&mut session, &baseline, resources, monitor)?;
    }
    Ok(MetricReport::new(
        STARTUP_WARMUPS,
        STARTUP_SAMPLES,
        samples,
        Some(3_000_000),
        None,
    ))
}

fn quick_open_metric(
    zec: &Path,
    root: &Path,
    resources: &mut ResourceTracker,
) -> Result<MetricReport> {
    let (mut session, baseline, _, monitor) = spawn_ready(zec, root, "bench-quick-open")?;
    let queries = expected_benchmark_quick_open_queries();
    ensure!(
        queries.len() == QUICK_SAMPLES,
        "quick-open spec query count differs"
    );
    let mut samples = Vec::with_capacity(QUICK_SAMPLES);
    let attempts = queries
        .iter()
        .take(QUICK_WARMUPS)
        .map(|query| (false, query))
        .chain(queries.iter().map(|query| (true, query)));
    for (measured, query) in attempts {
        let prompt = session.send_marked(CTRL_P)?;
        session.wait_contains("benchmark quick-open prompt", prompt, "Quick open:")?;
        let operation = session.paste_marked(&query.query)?;
        let expected_status = format!(
            "Quick open: {}  1/1  {}",
            query.query, query.expected_selected_path
        );
        let elapsed = session.wait_contains(
            "benchmark exact quick-open result",
            operation,
            &expected_status,
        )?;
        if measured {
            samples.push(elapsed);
        }
        let cancel = session.send_marked(ESC)?;
        session.wait_absent("benchmark quick-open cancel", cancel, "Quick open:")?;
    }
    quit_clean(&mut session, &baseline, resources, monitor)?;
    Ok(MetricReport::new(
        QUICK_WARMUPS,
        QUICK_SAMPLES,
        samples,
        Some(150_000),
        Some(500_000),
    ))
}

fn project_search_metric(
    zec: &Path,
    root: &Path,
    resources: &mut ResourceTracker,
) -> Result<(MetricReport, ProjectSearchObservation)> {
    let expected_rows = expected_benchmark_search_rows();
    ensure!(
        expected_rows.len() == fixture::SEARCH_RESULT_LIMIT,
        "project-search row oracle count differs"
    );
    let mut samples = Vec::with_capacity(PROJECT_SAMPLES);
    let mut observation = None;
    for index in 0..PROJECT_WARMUPS + PROJECT_SAMPLES {
        let (mut session, baseline, _, monitor) =
            spawn_ready(zec, root, &format!("bench-project-{index:02}"))?;
        let prompt = session.send_marked(ALT_F)?;
        session.wait_contains("benchmark project-search prompt", prompt, "Project search:")?;
        let operation = session.paste_marked("ALPHA1_BENCH_SEARCH")?;
        let first = &expected_rows[0];
        let expected_status = format!(
            "Project search: ALPHA1_BENCH_SEARCH  1/{}  {}:{}:{}  {}",
            fixture::BENCH_SEARCH_HITS,
            first.path,
            first.line,
            first.column,
            first.preview
        );
        let elapsed = session.wait_contains(
            "exact 1,000-hit completed project-search generation",
            operation,
            &expected_status,
        )?;
        if index == PROJECT_WARMUPS {
            observation = Some(observe_ordered_project_search_rows(
                &mut session,
                &expected_rows,
            )?);
        }
        if index >= PROJECT_WARMUPS {
            samples.push(elapsed);
        }
        quit_clean(&mut session, &baseline, resources, monitor)?;
    }
    Ok((
        MetricReport::new(
            PROJECT_WARMUPS,
            PROJECT_SAMPLES,
            samples,
            Some(5_000_000),
            None,
        ),
        observation.context("project-search produced no UI observation")?,
    ))
}

fn observe_ordered_project_search_rows(
    session: &mut PtySession,
    expected_rows: &[SearchResultReport],
) -> Result<ProjectSearchObservation> {
    let mut total_hits = None;
    let mut visible_results = Vec::with_capacity(expected_rows.len());
    for (index, expected) in expected_rows.iter().enumerate() {
        if index > 0 {
            let moved = session.send_marked(DOWN)?;
            let expected_status = format!(
                "Project search: ALPHA1_BENCH_SEARCH  {}/{}  {}:{}:{}  {}",
                index + 1,
                fixture::BENCH_SEARCH_HITS,
                expected.path,
                expected.line,
                expected.column,
                expected.preview
            );
            session.wait_contains(
                &format!("ordered project-search row {}", index + 1),
                moved,
                &expected_status,
            )?;
        }

        let (position, observed_total, row) =
            parse_project_search_status(session.screen(), expected)?;
        ensure!(
            position == index + 1,
            "project-search UI position {} differs from {}",
            position,
            index + 1
        );
        if let Some(previous_total) = total_hits {
            ensure!(
                observed_total == previous_total,
                "project-search UI total changed during traversal"
            );
        } else {
            total_hits = Some(observed_total);
        }
        visible_results.push(row);
    }

    let total_hits = total_hits.context("project-search UI traversal had no rows")?;
    ensure!(
        total_hits == fixture::BENCH_SEARCH_HITS,
        "project-search UI total differs from fixture"
    );
    ensure!(
        visible_results == expected_rows,
        "ordered project-search UI rows differ from spec"
    );
    Ok(ProjectSearchObservation {
        total_hits,
        visible_results,
    })
}

fn parse_project_search_status(
    screen: &vt100::Screen,
    expected: &SearchResultReport,
) -> Result<(usize, usize, SearchResultReport)> {
    const PREFIX: &str = "Project search: ALPHA1_BENCH_SEARCH  ";
    let contents = screen.contents();
    let status = contents
        .lines()
        .find_map(|line| line.strip_prefix(PREFIX))
        .context("rendered project-search status line was absent")?;
    let (position_and_total, result) = status
        .split_once("  ")
        .context("rendered project-search status lacked result separator")?;
    let (position, total_hits) = position_and_total
        .split_once('/')
        .context("rendered project-search status lacked position/total")?;
    let position = position
        .parse::<usize>()
        .context("rendered project-search position was not numeric")?;
    let total_hits = total_hits
        .parse::<usize>()
        .context("rendered project-search total was not exact numeric output")?;
    let (location, preview_and_footer) = result
        .split_once("  ")
        .context("rendered project-search status lacked preview separator")?;
    ensure!(
        preview_and_footer.starts_with(&expected.preview),
        "rendered project-search preview differs from exact oracle"
    );
    let footer = &preview_and_footer[expected.preview.len()..];
    let rendered_core = format!(
        "{PREFIX}{position_and_total}  {location}  {}",
        expected.preview
    );
    ensure!(
        rendered_core.is_ascii(),
        "benchmark project-search status oracle must be ASCII"
    );
    let remaining_columns = usize::from(e2e_support::COLS)
        .checked_sub(rendered_core.len())
        .context("project-search status core exceeded terminal width")?;
    // Every fixed benchmark row leaves at most six cells, so only the
    // ASCII prefix of ProjectSearchPrompt's default option summary can be
    // rendered. Alpha 3 deliberately exposes search modes before actions.
    const PROJECT_SEARCH_DEFAULT_OPTIONS_PREFIX: &str =
        "  [lit case:off word:off ignored:off open:off full:off]";
    ensure!(
        remaining_columns <= PROJECT_SEARCH_DEFAULT_OPTIONS_PREFIX.len(),
        "benchmark row left unverified columns after its option-summary prefix"
    );
    let expected_footer = PROJECT_SEARCH_DEFAULT_OPTIONS_PREFIX
        .get(..remaining_columns)
        .expect("ASCII option-summary clipping is a character boundary");
    ensure!(
        footer == expected_footer,
        "rendered project-search option suffix differed after terminal clipping: observed {footer:?}, expected {expected_footer:?}"
    );
    let preview = &preview_and_footer[..expected.preview.len()];
    let mut location = location.rsplitn(3, ':');
    let column = location
        .next()
        .context("rendered project-search status lacked column")?
        .parse::<u32>()
        .context("rendered project-search column was not numeric")?;
    let line = location
        .next()
        .context("rendered project-search status lacked line")?
        .parse::<u32>()
        .context("rendered project-search line was not numeric")?;
    let path = location
        .next()
        .context("rendered project-search status lacked path")?;
    ensure!(!path.is_empty(), "rendered project-search path was empty");

    let observed = SearchResultReport {
        path: path.to_owned(),
        line,
        column,
        preview: preview.to_owned(),
    };
    ensure!(
        &observed == expected,
        "rendered project-search row differs from exact oracle"
    );
    Ok((position, total_hits, observed))
}

fn in_flight_metrics(
    zec: &Path,
    root: &Path,
    resources: &mut ResourceTracker,
) -> Result<(MetricReport, MetricReport, MetricReport)> {
    let mut replace = Vec::with_capacity(IN_FLIGHT_ATTEMPTS);
    let mut cancel = Vec::with_capacity(IN_FLIGHT_ATTEMPTS);
    let mut quit = Vec::with_capacity(IN_FLIGHT_ATTEMPTS);
    let replacement_query_a = "ALPHA1_STALE_A";
    let replacement_query_b = "ALPHA1_STALE_B";
    let common_prefix_len = replacement_query_a
        .bytes()
        .zip(replacement_query_b.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    ensure!(
        replacement_query_a.is_char_boundary(common_prefix_len)
            && replacement_query_b.is_char_boundary(common_prefix_len),
        "replacement query common prefix is not UTF-8 aligned"
    );
    let removed_suffix_count = replacement_query_a[common_prefix_len..].chars().count();
    let inserted_suffix = &replacement_query_b[common_prefix_len..];

    for index in 0..IN_FLIGHT_ATTEMPTS {
        let (mut session, baseline, _, monitor) =
            spawn_ready(zec, root, &format!("bench-replace-{index:02}"))?;
        begin_in_flight_search(&mut session, replacement_query_a)?;
        for _ in 0..removed_suffix_count {
            session.send(b"\x7f")?;
        }
        let operation = session.paste_marked(inserted_suffix)?;
        replace.push(session.wait_contains(
            "exact replacement query result",
            operation,
            "Project search: ALPHA1_STALE_B  1/1  src/stale-b.txt:1:1  ALPHA1_STALE_B current query result",
        )?);
        quit_clean(&mut session, &baseline, resources, monitor)?;
    }

    for index in 0..IN_FLIGHT_ATTEMPTS {
        let (mut session, baseline, _, monitor) =
            spawn_ready(zec, root, &format!("bench-cancel-{index:02}"))?;
        begin_in_flight_search(&mut session, "ALPHA1_BENCH_SEARCH")?;
        let operation = session.send_marked(ESC)?;
        cancel.push(session.wait_absent(
            "cancel in-flight project search",
            operation,
            "Project search:",
        )?);
        quit_clean(&mut session, &baseline, resources, monitor)?;
    }

    for index in 0..IN_FLIGHT_ATTEMPTS {
        let (mut session, baseline, _, monitor) =
            spawn_ready(zec, root, &format!("bench-quit-{index:02}"))?;
        begin_in_flight_search(&mut session, "ALPHA1_BENCH_SEARCH")?;
        let operation = session.send_marked(CTRL_Q)?;
        let status = session.wait_exit()?;
        ensure!(status.success(), "in-flight Ctrl-Q failed: {status}");
        quit.push(operation.elapsed_us());
        session.assert_restored_and_joined(&baseline)?;
        resources.finish(monitor)?;
    }

    Ok((
        MetricReport::new(0, IN_FLIGHT_ATTEMPTS, replace, None, Some(250_000)),
        MetricReport::new(0, IN_FLIGHT_ATTEMPTS, cancel, None, Some(250_000)),
        MetricReport::new(0, IN_FLIGHT_ATTEMPTS, quit, None, Some(250_000)),
    ))
}

fn editing_metric(
    zec: &Path,
    root: &Path,
    resources: &mut ResourceTracker,
    payload_suffix_name: &str,
) -> Result<(MetricReport, InputTrace)> {
    let (mut session, baseline, _, monitor) = spawn_ready(zec, root, "bench-editing")?;
    let path = root.join("bench/large-100000-lines.txt");
    let original = fs::read(&path).context("read original 100,000-line file")?;
    let payload_suffix = match payload_suffix_name {
        "LF" => "\n",
        other => bail!("unsupported editing payload suffix {other}"),
    };
    quick_open(
        &mut session,
        "large-100000-lines.txt",
        "bench/large-100000-lines.txt",
    )?;
    let marker = "large-line-049999";
    session.send(CTRL_G)?;
    session.paste("50000:1")?;
    let positioned = session.send_marked(ENTER)?;
    session.wait_after(
        "100,000-line exact edit position",
        positioned,
        e2e_support::SCREEN_TIMEOUT,
        |screen| token_immediately_after_cursor(screen, marker),
    )?;
    let body_column = session.screen().cursor_position().1;

    let expected_ids = (1..=EDIT_SAMPLES)
        .map(|sequence| format!("EDIT_{sequence:04}"))
        .collect::<Vec<_>>();
    let warmup_ids = (0..EDIT_WARMUPS)
        .map(|sequence| format!("WARMUP_{sequence:02}"))
        .collect::<Vec<_>>();
    let mut sent = Vec::with_capacity(EDIT_SAMPLES);
    let mut applied = Vec::with_capacity(EDIT_SAMPLES);
    let mut samples = Vec::with_capacity(EDIT_SAMPLES);
    for index in 0..EDIT_WARMUPS + EDIT_SAMPLES {
        let id = if index < EDIT_WARMUPS {
            warmup_ids[index].clone()
        } else {
            expected_ids[index - EDIT_WARMUPS].clone()
        };
        let payload = format!("{id}{payload_suffix}");
        let operation = session.paste_marked(&payload)?;
        if index >= EDIT_WARMUPS {
            sent.push(id.clone());
        }
        let elapsed = session.wait_after(
            "sequential edit at cursor-relative expected cells",
            operation,
            e2e_support::SCREEN_TIMEOUT,
            |screen| {
                token_on_previous_editor_row(screen, &id, body_column)
                    && screen.contents().contains("large-100000-lines.txt+]")
            },
        )?;
        if index >= EDIT_WARMUPS {
            applied.push(id);
            samples.push(elapsed);
        }
    }

    let saved = session.send_marked(CTRL_S)?;
    session.wait_after(
        "editing buffer saved with dirty marker cleared",
        saved,
        e2e_support::SCREEN_TIMEOUT,
        |screen| {
            let contents = screen.contents();
            contents.contains("saved")
                && contents.contains("large-100000-lines.txt]")
                && !contents.contains("large-100000-lines.txt+]")
        },
    )?;
    let insertion_offset = original
        .windows(marker.len())
        .position(|window| window == marker.as_bytes())
        .context("line 50,000 marker is absent")?;
    let warmup_payload = warmup_ids
        .iter()
        .map(|id| format!("{id}{payload_suffix}"))
        .collect::<String>();
    let edit_payload = expected_ids
        .iter()
        .map(|id| format!("{id}{payload_suffix}"))
        .collect::<String>();
    let mut expected_disk =
        Vec::with_capacity(original.len() + warmup_payload.len() + edit_payload.len());
    expected_disk.extend_from_slice(&original[..insertion_offset]);
    expected_disk.extend_from_slice(warmup_payload.as_bytes());
    expected_disk.extend_from_slice(edit_payload.as_bytes());
    expected_disk.extend_from_slice(&original[insertion_offset..]);
    let disk = fs::read(&path).context("read saved editing buffer")?;
    ensure!(
        disk == expected_disk,
        "final 100,000-line Zed buffer bytes differ"
    );

    let observed_edit = &disk[insertion_offset + warmup_payload.len()
        ..insertion_offset + warmup_payload.len() + edit_payload.len()];
    let observed_edit =
        std::str::from_utf8(observed_edit).context("saved edit ID sequence is not UTF-8")?;
    let file_ids = observed_edit
        .split_terminator(payload_suffix)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    ensure!(file_ids == expected_ids, "saved edit ID sequence differs");
    quit_clean(&mut session, &baseline, resources, monitor)?;

    let dropped_count = expected_ids.len().saturating_sub(applied.len());
    let reordered = sent != applied || applied != file_ids;
    let trace = InputTrace {
        sent_input_ids: sent,
        applied_input_ids: applied,
        expected_input_ids: file_ids,
        dropped_count,
        reordered,
    };
    Ok((
        MetricReport::new(
            EDIT_WARMUPS,
            EDIT_SAMPLES,
            samples,
            Some(100_000),
            Some(500_000),
        ),
        trace,
    ))
}

fn save_metric(zec: &Path, root: &Path, resources: &mut ResourceTracker) -> Result<MetricReport> {
    let (mut session, baseline, _, monitor) = spawn_ready(zec, root, "bench-save")?;
    quick_open(&mut session, "save-5mib.txt", "bench/save-5mib.txt")?;
    session.send(CTRL_G)?;
    session.paste("1:1")?;
    let positioned = session.send_marked(ENTER)?;
    session.wait_contains("5 MiB save position", positioned, "Ln 1, Col 1")?;

    let path = root.join("bench/save-5mib.txt");
    let mut expected_disk = fs::read(&path).context("read original 5 MiB save file")?;
    ensure!(
        expected_disk.len() == 5 * 1024 * 1024,
        "save file size differs"
    );
    let mut samples = Vec::with_capacity(SAVE_SAMPLES);
    for index in 0..SAVE_WARMUPS + SAVE_SAMPLES {
        session.send(b"\x1b[H")?;
        session.send(DELETE)?;
        let replacement = if index % 2 == 0 { "a" } else { "b" };
        let edited = session.paste_marked(replacement)?;
        session.wait_after(
            "5 MiB buffer became dirty at the fixed cell",
            edited,
            e2e_support::SCREEN_TIMEOUT,
            |screen| {
                token_immediately_before_cursor(screen, replacement)
                    && screen.contents().contains("save-5mib.txt+]")
            },
        )?;
        ensure!(
            fs::read(&path)? == expected_disk,
            "disk bytes changed before Ctrl-S"
        );
        expected_disk[0] = replacement.as_bytes()[0];
        let operation = session.send_marked(CTRL_S)?;
        let ui_elapsed = session.wait_after(
            "5 MiB save completion with dirty marker cleared",
            operation,
            e2e_support::SCREEN_TIMEOUT,
            |screen| {
                let contents = screen.contents();
                contents.contains("saved")
                    && contents.contains("save-5mib.txt]")
                    && !contents.contains("save-5mib.txt+]")
            },
        )?;
        let disk = fs::read(&path).context("read 5 MiB save result")?;
        ensure!(disk == expected_disk, "full saved 5 MiB bytes differ");
        let elapsed = ui_elapsed.max(operation.elapsed_us());
        if index >= SAVE_WARMUPS {
            samples.push(elapsed);
        }
    }
    quit_clean(&mut session, &baseline, resources, monitor)?;
    Ok(MetricReport::new(
        SAVE_WARMUPS,
        SAVE_SAMPLES,
        samples,
        None,
        Some(2_000_000),
    ))
}

fn token_immediately_after_cursor(screen: &vt100::Screen, token: &str) -> bool {
    let (row, column) = screen.cursor_position();
    let Ok(width) = u16::try_from(token.len()) else {
        return false;
    };
    let (_, columns) = screen.size();
    if column.saturating_add(width) > columns {
        return false;
    }
    screen.contents_between(row, column, row, column + width) == token
}

fn token_on_previous_editor_row(screen: &vt100::Screen, token: &str, body_column: u16) -> bool {
    let (row, column) = screen.cursor_position();
    if row == 0 || column != body_column {
        return false;
    }
    let Ok(width) = u16::try_from(token.len()) else {
        return false;
    };
    let (_, columns) = screen.size();
    if body_column.saturating_add(width) > columns {
        return false;
    }
    screen.contents_between(row - 1, body_column, row - 1, body_column + width) == token
}

fn token_immediately_before_cursor(screen: &vt100::Screen, token: &str) -> bool {
    let (row, column) = screen.cursor_position();
    let Ok(width) = u16::try_from(token.len()) else {
        return false;
    };
    if column < width {
        return false;
    }
    screen.contents_between(row, column - width, row, column) == token
}
fn quick_open(session: &mut PtySession, query: &str, expected_path: &str) -> Result<()> {
    let prompt = session.send_marked(CTRL_P)?;
    session.wait_contains("benchmark quick-open prompt", prompt, "Quick open:")?;
    let queried = session.paste_marked(query)?;
    session.wait_contains("benchmark quick-open path", queried, expected_path)?;
    let opened = session.send_marked(ENTER)?;
    session.wait_after(
        "benchmark quick-open target editor frame",
        opened,
        e2e_support::SCREEN_TIMEOUT,
        |screen| {
            let contents = screen.contents();
            let (cursor_row, cursor_column) = screen.cursor_position();
            let (rows, columns) = screen.size();
            contents.contains(expected_path)
                && !contents.contains("Quick open:")
                && !screen.hide_cursor()
                && cursor_row < rows.saturating_sub(1)
                && cursor_column < columns
        },
    )?;
    Ok(())
}

fn begin_in_flight_search(session: &mut PtySession, query: &str) -> Result<()> {
    let prompt = session.send_marked(ALT_F)?;
    session.wait_contains("in-flight project-search prompt", prompt, "Project search:")?;
    let started = session.paste_marked(query)?;
    session.wait_contains(
        "project search entered Running state",
        started,
        &format!("Project search: {query}  searching…"),
    )?;
    Ok(())
}

fn quit_clean(
    session: &mut PtySession,
    baseline: &TerminalBaseline,
    resources: &mut ResourceTracker,
    monitor: ResourceMonitor,
) -> Result<()> {
    session.send(CTRL_Q)?;
    let status = session.wait_exit()?;
    ensure!(status.success(), "benchmark Ctrl-Q failed: {status}");
    session.assert_restored_and_joined(baseline)?;
    resources.finish(monitor)
}

#[cfg(test)]
mod tests {
    use std::{process::Command, time::Instant};

    use super::*;

    #[test]
    fn benchmark_spec_is_fully_consumed_and_fixed() {
        let oracle = benchmark_oracle().expect("parse benchmark oracle");
        verify_benchmark_oracle(&oracle).expect("verify every benchmark oracle field");
    }

    #[test]
    fn project_search_status_parser_handles_120_columns_and_rejects_suffix() {
        let mut expected = SearchResultReport {
            path: "bench/search-0099.txt".to_owned(),
            line: 1,
            column: 11,
            preview: String::new(),
        };
        let header = format!(
            "Project search: ALPHA1_BENCH_SEARCH  100/1000  {}:{}:{}  ",
            expected.path, expected.line, expected.column
        );
        let preview_width = usize::from(e2e_support::COLS)
            .checked_sub(header.len())
            .expect("status header fits the fixed terminal");
        expected.preview = "x".repeat(preview_width);
        let status = format!("{header}{}", expected.preview);
        assert_eq!(status.len(), usize::from(e2e_support::COLS));

        let mut parser = vt100::Parser::new(e2e_support::ROWS, e2e_support::COLS, 0);
        parser.process(format!("\x1b[2J\x1b[40;1H{status}").as_bytes());
        let (position, total_hits, observed) =
            parse_project_search_status(parser.screen(), &expected)
                .expect("parse an exact full-width status row");
        assert_eq!((position, total_hits), (100, 1_000));
        assert_eq!(observed, expected);

        let expected = SearchResultReport {
            path: "bench/search-0009.txt".to_owned(),
            line: 1,
            column: 11,
            preview: "row 0009: ALPHA1_BENCH_SEARCH result 0009".to_owned(),
        };
        let core = format!(
            "Project search: ALPHA1_BENCH_SEARCH  10/1000  {}:{}:{}  {}",
            expected.path, expected.line, expected.column, expected.preview
        );
        assert_eq!(core.len(), 115);
        let status_with_clipped_options = format!("{core}  [li");
        assert_eq!(
            status_with_clipped_options.len(),
            usize::from(e2e_support::COLS)
        );
        let mut parser = vt100::Parser::new(e2e_support::ROWS, e2e_support::COLS, 0);
        parser.process(format!("\x1b[2J\x1b[40;1H{status_with_clipped_options}").as_bytes());
        let (position, total_hits, observed) =
            parse_project_search_status(parser.screen(), &expected)
                .expect("accept the exact option summary clipped at 120 columns");
        assert_eq!((position, total_hits), (10, 1_000));
        assert_eq!(observed, expected);

        let status_with_bad_suffix = format!("{core}  [lX");
        let mut parser = vt100::Parser::new(e2e_support::ROWS, e2e_support::COLS, 0);
        parser.process(format!("\x1b[2J\x1b[40;1H{status_with_bad_suffix}").as_bytes());
        let error = parse_project_search_status(parser.screen(), &expected)
            .expect_err("unexpected rendered suffix must be rejected");
        assert!(
            error
                .to_string()
                .contains("rendered project-search option suffix differed")
        );
    }

    #[test]
    fn edit_cell_predicate_checks_the_previous_editor_row_without_the_gutter() {
        let mut parser = vt100::Parser::new(3, 10, 0);
        parser.process(b"\x1b[1;1H  ABCDE   \x1b[2;1H  marker  \x1b[2;3H");
        let screen = parser.screen();

        assert_eq!(screen.cursor_position(), (1, 2));
        assert!(token_on_previous_editor_row(screen, "ABCDE", 2));
        assert!(!token_on_previous_editor_row(screen, "ABCXE", 2));
        assert!(token_immediately_after_cursor(screen, "marker"));
    }

    #[test]
    fn proc_events_capture_a_short_lived_descendant() {
        let mut events = ProcEventSocket::subscribe().expect("subscribe before root spawn");
        let mut root = Command::new("/bin/sh")
            .args(["-c", "(/bin/true) & wait"])
            .spawn()
            .expect("spawn synthetic root process");
        let root_pid = i32::try_from(root.id()).expect("root PID fits pid_t");
        let status = root.wait().expect("reap synthetic root process");
        assert!(status.success());

        let deadline = Instant::now() + Duration::from_secs(1);
        let mut edges = Vec::new();
        loop {
            events
                .drain_fork_edges(&mut edges)
                .expect("drain loss-free proc events");
            if observed_descendant_count(root_pid, &edges) > 0 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "short-lived descendant was absent from proc events: {edges:?}"
            );
            thread::sleep(Duration::from_millis(1));
        }
        assert!(
            descendant_process_count(root_pid).is_err(),
            "synthetic root unexpectedly remained in /proc"
        );
        events.unsubscribe().expect("send explicit IGNORE");
        assert!(!events.subscribed);
    }
}
