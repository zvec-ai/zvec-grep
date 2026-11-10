#!/usr/bin/env python3
"""Run the private module test, preserving measurements and exclusion evidence."""
import ctypes
import hashlib
import json
import multiprocessing
import os
from pathlib import Path
import platform
import plistlib
import subprocess
import threading
import time

import psutil
from evidence import digest, publish_round, save
from execution import validate_controls
from measurement import POLICY, PHASES, checked_accounting, background_summary, case_path, profile, resource_summary as summarize_resources, fingerprint

ROOT = Path(__file__).resolve().parents[2]
TEST = "models::tests::model_micro_bench::fixed_shape::local_model_shape"


def clock():
    # macOS Python <3.10 monotonic() has a process-local epoch. All workers
    # must use the same OS clock to join GPU/background samples with phases.
    if hasattr(time, "CLOCK_MONOTONIC"):
        return time.clock_gettime(time.CLOCK_MONOTONIC)
    return time.monotonic()


def command(*args):
    return subprocess.check_output(args, cwd=ROOT, text=True).strip()


class Usage(ctypes.Structure):
    _fields_ = [("uuid", ctypes.c_uint8 * 16)] + [(name, ctypes.c_uint64) for name in (
        "user_ns", "system_ns", "pkg_wakeups", "interrupt_wakeups", "pageins",
        "wired", "rss", "footprint", "started", "exited",
        "child_user", "child_system", "child_pkg", "child_interrupt", "child_pageins", "child_elapsed",
        "disk_read", "disk_write", "qos_default", "qos_maintenance", "qos_background",
        "qos_utility", "qos_legacy", "qos_user_initiated", "qos_user_interactive",
        "billed_system", "serviced_system", "logical_writes", "peak_footprint",
        "instructions", "cycles", "billed_energy", "serviced_energy", "interval_peak", "runnable")]


if platform.system() == "Darwin":
    LIBPROC = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
    LIBPROC.proc_pid_rusage.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_void_p]
    LIBPROC.proc_pid_rusage.restype = ctypes.c_int
    class Timebase(ctypes.Structure):
        _fields_ = [("numer", ctypes.c_uint32), ("denom", ctypes.c_uint32)]
    timebase = Timebase()
    ctypes.CDLL("/usr/lib/libSystem.B.dylib").mach_timebase_info(ctypes.byref(timebase))
    TICK_SECONDS = timebase.numer/timebase.denom/1e9
else:
    LIBPROC = None


def sample(process, started, phase):
    before = clock()
    if LIBPROC:
        # psutil.cpu_times()/num_threads() enumerate Mach threads and can stall
        # for seconds under an oversubscribed ORT pool. rusage is process-wide.
        usage = Usage()
        if LIBPROC.proc_pid_rusage(process.pid, 4, ctypes.byref(usage)):
            raise psutil.NoSuchProcess(process.pid)
        after = clock()
        return dict(at_seconds=(before+after)/2-started, collection_seconds=after-before, phase=phase, rss_bytes=usage.rss,
                    cpu_seconds=(usage.user_ns+usage.system_ns)*TICK_SECONDS, threads=None,
                    physical_footprint_bytes=usage.footprint,
                    lifetime_peak_footprint_bytes=usage.peak_footprint, gpu_memory_bytes=None)
    cpu = process.cpu_times()
    record = dict(at_seconds=clock()-started, phase=phase,
                  rss_bytes=process.memory_info().rss, cpu_seconds=cpu.user+cpu.system,
                  threads=process.num_threads(), physical_footprint_bytes=None,
                  lifetime_peak_footprint_bytes=None, gpu_memory_bytes=None)
    return record


def background(skip=()):
    # Names only: never persist unrelated processes' command lines or environment.
    processes = []
    for p in psutil.process_iter(["pid", "name", "create_time"]):
        try:
            info = p.info
            if p.pid in skip:
                continue
            cpu = p.cpu_times()
            if cpu:
                processes.append(dict(pid=info["pid"], name=info["name"], create_time=info["create_time"], cpu_seconds=cpu.user+cpu.system))
        except psutil.Error:
            pass
    return dict(at=clock(), load_average=os.getloadavg(),
                available_memory_bytes=psutil.virtual_memory().available, processes=processes)


def background_worker(stop, child_pid, runner_pid, path):
    # Some psutil Mach calls hold the Python GIL while enumerating threads.
    # A separate process keeps those calls from stalling the 20ms sampler.
    with path.open("w") as stream:
        while not stop.wait(1):
            value = background(skip=(child_pid, runner_pid, os.getpid()))
            value["scanner_cpu_seconds"] = time.process_time()
            stream.write(json.dumps(value)+"\n")
            stream.flush()


def gpu_worker(stop, child_pid, device, path):
    with path.open("w") as stream:
        while not stop.is_set():
            record = dict(at=clock(), device_utilization_percent=None,
                          process_memory_bytes=None, unified_device_memory_bytes=None)
            try:
                if platform.system() == "Darwin":
                    records = plistlib.loads(subprocess.check_output(
                        ["ioreg", "-r", "-c", "AGXAccelerator", "-a"], timeout=2))
                    stats = records[0].get("PerformanceStatistics", {})
                    record.update(device_utilization_percent=stats.get("Device Utilization %"),
                                  unified_device_memory_bytes=stats.get("In use system memory"),
                                  scope="device-wide; unified memory; includes other applications")
                elif device == "cuda":
                    utilization = subprocess.check_output(["nvidia-smi", "--query-gpu=utilization.gpu", "--format=csv,noheader,nounits"], text=True, timeout=2)
                    per_gpu = [float(x) for x in utilization.splitlines()]
                    record.update(device_utilization_percent=max(per_gpu, default=None),
                                  device_utilization_by_gpu_percent=per_gpu,
                                  scope="device-wide utilization (maximum across GPUs); per-process memory")
                    memory = subprocess.check_output(["nvidia-smi", "--query-compute-apps=pid,used_gpu_memory", "--format=csv,noheader,nounits"], text=True, timeout=2)
                    record["process_memory_bytes"] = sum(float(line.split(",")[1])*1024**2 for line in memory.splitlines() if int(line.split(",")[0]) == child_pid)
            except (OSError, subprocess.SubprocessError, ValueError, IndexError) as error:
                record["unavailable_reason"] = str(error)
            stream.write(json.dumps(record)+"\n"); stream.flush()
            stop.wait(0.5)


def resource_summary(samples):
    return summarize_resources(samples, psutil.cpu_count() or 1)


def machine_info():
    """Collect hardware labels before measurement, without serial numbers or UUIDs."""
    machine = dict(os=platform.platform(), architecture=platform.machine(),
                   logical_cpus=psutil.cpu_count(), physical_cpus=psutil.cpu_count(logical=False),
                   ram_bytes=psutil.virtual_memory().total, cpu=platform.processor() or None,
                   model=None, model_identifier=None, gpus=[])
    if platform.system() == "Darwin":
        try:
            data = json.loads(subprocess.check_output(
                ["system_profiler", "-json", "SPHardwareDataType", "SPDisplaysDataType"],
                text=True, timeout=30))
            hardware = data.get("SPHardwareDataType", [{}])[0]
            machine.update(model=hardware.get("machine_name"), model_identifier=hardware.get("machine_model"),
                           cpu=hardware.get("chip_type") or hardware.get("cpu_type") or machine["cpu"])
            for gpu in data.get("SPDisplaysDataType", []):
                name = gpu.get("sppci_model")
                if name:
                    cores = gpu.get("sppci_cores")
                    machine["gpus"].append(f"{name} ({cores} cores)" if cores else name)
        except (OSError, subprocess.SubprocessError, ValueError, IndexError):
            pass
    elif platform.system() == "Linux":
        try:
            machine["model"] = Path("/sys/class/dmi/id/product_name").read_text().strip() or None
        except OSError:
            pass
        try:
            for line in Path("/proc/cpuinfo").read_text().splitlines():
                if line.lower().startswith("model name"):
                    machine["cpu"] = line.split(":", 1)[1].strip()
                    break
        except OSError:
            pass
        try:
            names = subprocess.check_output(
                ["nvidia-smi", "--query-gpu=name", "--format=csv,noheader"],
                text=True, stderr=subprocess.DEVNULL, timeout=5)
            machine["gpus"] = [name.strip() for name in names.splitlines() if name.strip()]
        except (OSError, subprocess.SubprocessError):
            pass
    return machine


def environment(build_json):
    for key in ("ORT_DYLIB_PATH", "ZVEC_GREP_NATIVE_LIB_DIR"):
        if os.environ.get(key):
            raise RuntimeError(f"Unset {key}; initial baseline requires the pinned packaged runtime")
    records = [json.loads(line) for line in build_json.read_text().splitlines() if line.startswith("{")]
    candidates = [r for r in records if r.get("reason") == "compiler-artifact" and r.get("executable")
                  and r["target"]["name"] == "zg_engine" and r["profile"]["test"]]
    if not candidates:
        raise RuntimeError("No zg_engine test executable in build JSON")
    artifact = candidates[-1]
    binary = Path(artifact["executable"])
    if artifact["profile"]["opt_level"] != "3" or set(artifact["features"])-{"default", "cuda"}:
        raise RuntimeError("Expected release profile with only default or CUDA features")
    libname = "libonnxruntime.1.dylib" if platform.system() == "Darwin" else "libonnxruntime.so.1"
    library = binary.parent/libname
    # Version introspection only; all inference is in the Rust model module.
    lib = ctypes.CDLL(str(library))
    class ApiBase(ctypes.Structure):
        _fields_ = [("get_api", ctypes.c_void_p), ("get_version", ctypes.CFUNCTYPE(ctypes.c_char_p))]
    lib.OrtGetApiBase.restype = ctypes.POINTER(ApiBase)
    version = lib.OrtGetApiBase().contents.get_version().decode()
    if version != "1.21.0":
        raise RuntimeError(f"Expected ORT 1.21.0, found {version}")
    env = dict(os.environ)
    links = [str(binary.parent), str(binary.parent.parent)]
    for record in records:
        if record.get("reason") == "build-script-executed":
            links.extend(link.split("=", 1)[-1] for link in record.get("linked_paths", []))
    env["DYLD_FALLBACK_LIBRARY_PATH" if platform.system() == "Darwin" else "LD_LIBRARY_PATH"] = os.pathsep.join(links)
    source_files = sorted(p for p in (ROOT/"crates").rglob("*") if p.suffix in (".rs", ".toml"))
    source_files += sorted((ROOT/"micro-bench/model").glob("*.*"))
    source_files += [ROOT/"Cargo.toml", ROOT/"scripts/onnxruntime-distributions.json"]
    source_hashes = {str(p.relative_to(ROOT)): digest(p) for p in source_files}
    info = dict(source_commit=command("git", "rev-parse", "HEAD"),
                dirty_status=command("git", "status", "--short"),
                tracked_patch_sha256=hashlib.sha256(command("git", "diff", "HEAD").encode()).hexdigest(),
                source_sha256=source_hashes, cargo_lock_sha256=digest(ROOT/"Cargo.lock"),
                build_json_sha256=digest(build_json), binary_sha256=digest(binary), build_profile=artifact["profile"],
                features=artifact["features"], rustc=command("rustc", "+1.98.0", "-vV"),
                build_environment={k:os.environ.get(k) for k in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_PROFILE_RELEASE_LTO", "CARGO_PROFILE_RELEASE_CODEGEN_UNITS")},
                ort=dict(path=str(library), sha256=digest(library), version=version),
                machine=machine_info(),
                command=[str(binary), TEST, "--ignored", "--exact", "--nocapture", "--test-threads=1"])
    return env, info


def run(args, budget, env, info):
    key = dict(model=args.model,device=args.device,budget=str(budget),input_tokens=args.input_tokens,batch_size=args.batch_size)
    destination = case_path(args.output,key,getattr(args,"round_index",1))
    if destination.exists() and any(destination.iterdir()):
        destination.rename(destination.with_name(destination.name+f".incomplete-{time.time_ns()}"))
    destination.mkdir(parents=True, exist_ok=True)
    env = dict(env, ZG_MODEL_BENCH_CACHE=str(args.cache.resolve()), ZG_MODEL_BENCH_CONCURRENCY=str(budget),
               ZG_MODEL_BENCH_MODEL=args.model, ZG_MODEL_BENCH_DEVICE=args.device)
    # Default means the actual backend default, not the interactive shell's index override.
    for key in ("ZVEC_GREP_EMBEDDING_CONCURRENCY", "ZVEC_GREP_LLAMA_CONTEXT_PARALLELISM"):
        env.pop(key, None)
    workload = profile(args.smoke)
    env.update(ZG_MODEL_BENCH_TOKENS=str(args.input_tokens), ZG_MODEL_BENCH_BATCH_SIZE=str(args.batch_size),
               ZG_MODEL_BENCH_SECONDS=str(workload["duration_seconds"]),
               ZG_MODEL_BENCH_MAX_SECONDS=str(workload["maximum_duration_seconds"]),
               ZG_MODEL_BENCH_MIN_BATCHES=str(workload["minimum_batches"]))
    clients = getattr(args, "clients", None)
    server_concurrency = getattr(args, "server_concurrency", None)
    ort_thread_budget = getattr(args, "ort_thread_budget", None)
    env["ZG_MODEL_BENCH_RUNTIME_MODE"] = "server" if server_concurrency is not None else "direct"
    if server_concurrency is not None:
        env["ZVEC_GREP_EMBEDDING_CONCURRENCY"] = str(server_concurrency)
    env.pop("ZVEC_GREP_ORT_THREAD_BUDGET", None)
    if ort_thread_budget is not None:
        env["ZVEC_GREP_ORT_THREAD_BUDGET"] = str(ort_thread_budget)
    env.pop("ZG_MODEL_BENCH_CLIENTS", None)
    if clients is not None:
        env["ZG_MODEL_BENCH_CLIENTS"] = str(clients)
    state = dict(phase="startup", result=None)
    events, samples, backgrounds, read_errors = [], [], [], []
    requests = {}
    backgrounds.append(background())
    started = clock()
    runner_cpu = time.process_time()
    child = subprocess.Popen(info["command"], cwd=ROOT, env=env, stdout=subprocess.PIPE,
                             stderr=subprocess.PIPE, text=True, bufsize=1)
    process = psutil.Process(child.pid)
    sampling_lock = threading.RLock()

    def collect(phase=None):
        with sampling_lock:
            samples.append(sample(process, started, state["phase"] if phase is None else phase))

    def stdout():
        try:
            with (destination/"stdout.log").open("w") as log, (destination/"requests.jsonl").open("w") as rows:
                for line in child.stdout:
                    log.write(line)
                    log.flush()
                    for marker in ("ZG_BENCH_EVENT=", "ZG_BENCH_ROWS=", "ZG_BENCH_RESULT="):
                        if marker in line:
                            value = json.loads(line.split(marker, 1)[1])
                            if marker == "ZG_BENCH_EVENT=":
                                with sampling_lock:
                                    value["observed_monotonic"] = clock()
                                    try:
                                        collect(state["phase"])
                                        collect(value["phase"])
                                    except psutil.Error:
                                        pass
                                    state["phase"] = value["phase"]
                                    events.append(value)
                            elif marker == "ZG_BENCH_ROWS=":
                                if value["phase"] in requests:
                                    read_errors.append("duplicate request phase")
                                requests[value["phase"]] = value["rows"]
                                rows.write(json.dumps(value)+"\n")
                            else:
                                if isinstance(value,dict):
                                    state["result"] = value
                                else:
                                    read_errors.append("benchmark result is not an object")
        except Exception as error:
            read_errors.append(repr(error))

    def stderr():
        with (destination/"stderr.log").open("w") as log:
            for line in child.stderr:
                log.write(line)

    readers = [threading.Thread(target=stdout), threading.Thread(target=stderr)]
    for reader in readers:
        reader.start()
    context = multiprocessing.get_context("spawn")
    stop_background = context.Event()
    background_path = destination/"background-scans.jsonl"
    scanner = context.Process(target=background_worker,
                              args=(stop_background, child.pid, os.getpid(), background_path))
    scanner.start()
    gpu_path = destination/"gpu.jsonl"
    gpu_scanner = context.Process(target=gpu_worker, args=(stop_background, child.pid, args.device, gpu_path))
    gpu_scanner.start()
    timed_out = False
    while child.poll() is None:
        try:
            collect()
        except psutil.Error:
            pass
        if clock()-started > args.timeout:
            child.kill()
            timed_out = True
        time.sleep(0.02)
    for reader in readers:
        reader.join()
    stop_background.set()
    scanner.join(timeout=5)
    gpu_scanner.join(timeout=5)
    if gpu_scanner.is_alive():
        gpu_scanner.terminate(); gpu_scanner.join()
        read_errors.append("GPU sampler did not stop")
    if scanner.is_alive():
        scanner.terminate()
        scanner.join()
        read_errors.append("background scanner did not stop within 5s")
    if scanner.exitcode:
        read_errors.append("background scanner exited unsuccessfully")
    if background_path.exists():
        backgrounds.extend(json.loads(line) for line in background_path.read_text().splitlines())
    backgrounds.append(background())
    samples.sort(key=lambda s:s["at_seconds"])
    exclusion = []
    background_intervals = []
    for before, after in zip(backgrounds, backgrounds[1:]):
        previous = {p["pid"]:p for p in before["processes"]}
        active = []
        for p in after["processes"]:
            old = previous.get(p["pid"])
            if old and p["pid"] not in (os.getpid(), child.pid, scanner.pid, gpu_scanner.pid) and old["name"] == p["name"] and old["create_time"] == p["create_time"]:
                cores = max(0, p["cpu_seconds"]-old["cpu_seconds"])/(after["at"]-before["at"])
                if cores > 0.01:
                    active.append(dict(pid=p["pid"], name=p["name"], cpu_cores=cores))
        active.sort(key=lambda p:p["cpu_cores"], reverse=True)
        background_intervals.append(dict(at_seconds=after["at"]-started,
                                         start_seconds=before["at"]-started, end_seconds=after["at"]-started,
                                         aggregate_cpu_cores=sum(p["cpu_cores"] for p in active), top_processes=active[:10]))
    if args.smoke:
        exclusion.append("smoke profile: insufficient duration/sample count")
    result = state["result"]
    accounting_errors = checked_accounting(result,requests,args.input_tokens,args.batch_size)
    accounting_errors.extend(validate_controls(result, clients, server_concurrency, ort_thread_budget))
    if result and result.get("fixed_batch", False) is not True:
        accounting_errors.append("recorded batching policy differs from requested profile")
    passed = bool(child.returncode == 0 and not timed_out and not read_errors and not accounting_errors
                  and result and result["protocol_pass"])
    if not passed:
        exclusion.append("protocol failure or incomplete run")
    measured = result.get("embedding",{}) if result and isinstance(result.get("embedding"),dict) else {}
    if result and not measured.get("minimum_sample_met",False):
        exclusion.append("minimum batch sample target not met")
    gaps = [b["at_seconds"]-a["at_seconds"] for a,b in zip(samples,samples[1:])]
    inference_phases = set(PHASES)
    inference_gaps = [b["at_seconds"]-a["at_seconds"] for a,b in zip(samples,samples[1:])
                      if a["phase"] in inference_phases or b["phase"] in inference_phases]
    max_gap = POLICY["maximum_sample_gap_seconds"]
    if max(inference_gaps, default=0) > max_gap:
        exclusion.append(f"inference resource sampling gap exceeded {max_gap * 1000:g} ms")
    if not inference_phases.issubset({s["phase"] for s in samples}):
        exclusion.append("missing inference resource phase samples")
    gpu_samples = [json.loads(line) for line in gpu_path.read_text().splitlines()] if gpu_path.exists() else []
    for sample_value in gpu_samples:
        preceding = [e for e in events if e["observed_monotonic"] <= sample_value["at"]]
        sample_value["phase"] = preceding[-1]["phase"] if preceding else "startup"
    gpu_values = [s["device_utilization_percent"] for s in gpu_samples if s["phase"] in inference_phases and isinstance(s["device_utilization_percent"], (int,float))]
    save(destination/"gpu-phases.json", gpu_samples)
    resources = resource_summary(samples)
    background_by_phase = background_summary(background_intervals, events, started)
    phase_quality = {}
    for name in PHASES:
        phase_samples = [s for s in samples if s["phase"] == name]
        phase_gaps = [b["at_seconds"]-a["at_seconds"] for a,b in zip(phase_samples, phase_samples[1:])]
        phase_quality[name] = dict(background=background_by_phase.get(name),
            sampling_valid=len(phase_samples) >= 2 and max(phase_gaps, default=0) <= max_gap,
            cpu_peak_valid=resources.get(name, {}).get("peak_cpu_percent_one_core") is not None)
    output = dict(schema_version=5, provenance=info,
                  experiment_fingerprint=getattr(args, "experiment_fingerprint", None),
                  case_fingerprint=getattr(args, "case_fingerprint", fingerprint(dict(binary=info["binary_sha256"], workload=workload, budget=budget))),
                  round_index=getattr(args, "round_index", 1), phase_quality=phase_quality,
                  configuration=dict(embedding_concurrency=budget,
                  model=args.model, device=args.device, input_length_semantics="nominal",
                  input_tokens=args.input_tokens,batch_size=args.batch_size,
                  fixed_batch=True,
                  clients=clients, server_concurrency=server_concurrency, ort_thread_budget=ort_thread_budget,
                  profile=workload["name"], cache=str(args.cache.resolve())),
                  exit_code=child.returncode, hard_timeout=timed_out, protocol_pass=passed, reader_errors=read_errors, accounting_errors=accounting_errors,
                  eligible_for_comparison=passed and not exclusion, exclusion_reasons=exclusion,
                  benchmark=result, resources=resources,
                  sampler=dict(target_interval_ms=20, maximum_gap_ms=1000*max(gaps,default=0),
                               maximum_inference_gap_ms=1000*max(inference_gaps,default=0),
                               runner_cpu_seconds=time.process_time()-runner_cpu,
                               background_scanner_cpu_seconds=max((b.get("scanner_cpu_seconds",0) for b in backgrounds), default=0),
                               total_wall_seconds=clock()-started),
                  thermal_state=None, gpu_memory_bytes=None,
                  gpu=dict(scope="device-wide; not process utilization", samples=len(gpu_values),
                           phases={phase:dict(samples=len(values), mean_percent=sum(values)/len(values) if values else None,
                                               peak_percent=max(values,default=None))
                                   for phase in PHASES
                                   for values in [[s["device_utilization_percent"] for s in gpu_samples
                                                   if s["phase"] == phase and isinstance(s["device_utilization_percent"], (int,float))]]},
                           mean_percent=sum(gpu_values)/len(gpu_values) if gpu_values else None,
                           peak_percent=max(gpu_values,default=None),
                           process_memory_peak_bytes=max((s["process_memory_bytes"] for s in gpu_samples
                               if s["phase"] in inference_phases and s["process_memory_bytes"] is not None),default=None)))
    publish_round(destination, output, events, samples, dict(
        snapshots=backgrounds, intervals=background_intervals, phases=background_by_phase))
    print(json.dumps(dict(model=args.model, device=args.device, budget=budget, protocol_pass=passed, eligible=output["eligible_for_comparison"],
                         exclusions=exclusion, result=str(destination/"result.json"))), flush=True)
    return passed


def inventory(env, info):
    cmd = list(info["command"]); cmd[1] = "models::tests::model_micro_bench::support::inventory"
    output = subprocess.check_output(cmd, env=env, cwd=ROOT, text=True)
    return json.loads(next(line.split("ZG_BENCH_INVENTORY=",1)[1] for line in output.splitlines() if "ZG_BENCH_INVENTORY=" in line))


if __name__ == "__main__":
    raise SystemExit("Use suite.py to run the benchmark; use --models and --devices for a single case.")
