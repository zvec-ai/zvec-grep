"""One fixed seven-column performance table."""
import html
import json
import math
from pathlib import Path
import statistics
import sys

from measurement import case_identity, case_path, fingerprint
from evidence import digest, save
from execution import execution_label, execution_records

REPORT_HASH = fingerprint([digest(Path(__file__)), digest(Path(__file__).with_name("execution.py"))])
HEADERS = ["模型 / L / B / C / 设备",
           "批延迟 p50 ms", "吞吐 vectors/s", "峰值 RSS MiB",
           "进程显存峰值 MiB", "CPU 均值 / 1秒峰值 %", "GPU 全卡均值 / 峰值 %"]


def get(value, *path):
    for key in path:
        if not isinstance(value, dict): return None
        value = value.get(key)
    return value


def distribution(values):
    values = [v for v in values if isinstance(v, (int,float)) and not isinstance(v,bool) and math.isfinite(v)]
    return dict(n=len(values), median=statistics.median(values), minimum=min(values), maximum=max(values)) if values else dict(n=0,median=None,minimum=None,maximum=None)


def display(stats, digits=2, spread=True):
    if not stats["n"]: return "N/A"
    value = f'{stats["median"]:.{digits}f}'
    if spread and stats["n"] > 1: value += f' [{stats["minimum"]:.{digits}f}–{stats["maximum"]:.{digits}f}]'
    return value


def case_label(case, execution=()):
    label = (f'{case["model"].removeprefix("local/")} · L={case["input_tokens"]} '
             f'· B={case["batch_size"]} · C={case["budget"]} · {case["device"].upper()}')
    observed = execution_label(execution)
    return label + (" · " + observed if observed else "")


def summarize_case(root, case, rounds, experiment):
    folder = case_path(root,case)
    paths = sorted(p for p in folder.glob("r*/result.json") if p.parent.name[1:].isdigit())
    signatures = [(str(p.relative_to(root)),p.stat().st_size,p.stat().st_mtime_ns) for p in paths]
    status_path = folder/"case-status.json"
    status = json.loads(status_path.read_text()) if status_path.exists() else None
    if status:
        if (status.get("status") != "unsupported" or status.get("case_key") != case
                or status.get("experiment_fingerprint") != experiment
                or status.get("case_fingerprint") != case_identity(experiment,case,None)):
            raise RuntimeError(f"Unbound capability result: {status_path}")
        if paths:
            raise RuntimeError(f"Capability skip conflicts with measured rounds: {folder}")
        signatures.append((str(status_path.relative_to(root)),status_path.stat().st_size,status_path.stat().st_mtime_ns))
    cached = folder/"result.json"
    if cached.exists():
        previous = json.loads(cached.read_text())
        if (previous.get("schema_version") == 5 and previous.get("renderer_sha256") == REPORT_HASH
                and previous.get("case_key") == case and previous.get("experiment_fingerprint") == experiment
                and previous.get("signatures") == [list(s) for s in signatures]
                and previous.get("expected_rounds") == rounds):
            return previous["table_row"],previous
    records = []
    for path in paths:
        index = int(path.parent.name[1:])
        record = json.loads(path.read_text())
        if (record.get("schema_version") != 5 or not 1 <= index <= rounds
                or record.get("experiment_fingerprint") != experiment
                or record.get("case_fingerprint") != case_identity(experiment,case,index)):
            raise RuntimeError(f"Cannot summarize another experiment or schema: {path}")
        records.append(record)
    successful = [r for r in records if r.get("protocol_pass")]
    metrics = {}

    def metric(name,*path,digits=2,scale=1,spread=True):
        values = [get(r,*path) for r in successful]
        metrics[name] = distribution([v*scale for v in values if isinstance(v,(int,float)) and not isinstance(v,bool)])
        return display(metrics[name],digits,spread)

    latencies = {p:metric(f"batch_{p}","benchmark","embedding","call_latency",f"{p}_ms",digits=3,spread=False)
                 for p in ("p50","p95","p99")}
    throughput = metric("vectors_s","benchmark","embedding","vectors_per_second")
    metric("nominal_tokens_s","benchmark","embedding","nominal_tokens_per_second")
    rss = metric("rss_peak_mib","resources","embedding","sampled_peak_rss_bytes",scale=1/1024**2)
    vram = metric("vram_peak_mib","gpu","process_memory_peak_bytes",scale=1/1024**2)
    cpu = (metric("cpu_mean","resources","embedding","mean_cpu_percent_one_core")+" / "
           +metric("cpu_peak_1s","resources","embedding","peak_cpu_percent_one_core"))
    gpu = (metric("gpu_mean","gpu","phases","embedding","mean_percent")+" / "
           +metric("gpu_peak","gpu","phases","embedding","peak_percent"))
    failures = len(records)-len(successful)
    hard_timeouts = sum(bool(r.get("hard_timeout")) for r in records)
    smoke_rounds = sum(get(r,"configuration","profile") == "smoke" for r in records)
    sample_target_missed = sum(get(r,"benchmark","embedding","minimum_sample_met") is False for r in records)
    background = sum(bool(get(r,"phase_quality","embedding","background","warning")) for r in records)
    sampling_bad = sum(get(r,"phase_quality","embedding","sampling_valid") is not True for r in successful)
    cpu_unavailable = sum(get(r,"resources","embedding","peak_cpu_percent_one_core") is None for r in successful)
    eligible = sum(bool(r.get("eligible_for_comparison")) for r in records)
    counts = {key:sum(get(r,"benchmark","embedding",key) or 0 for r in records) for key in ("completed","errors","timeout")}
    accounting_complete = bool(records) and all(get(r,"benchmark","embedding",key) is not None
        for r in records for key in ("issued","completed","errors","timeout"))
    quality_notes = []
    if status:
        quality_notes.append("不支持："+status["reason"])
    else:
        if len(records) < rounds:
            quality_notes.append(f"未完成：已采集 {len(records)}/{rounds} 轮")
        if failures:
            quality_notes.append(f"执行/采集失败 {failures} 轮")
        if hard_timeouts:
            quality_notes.append(f"进程硬超时 {hard_timeouts} 轮，批次计数可能不完整")
        if sample_target_missed:
            quality_notes.append(f"样本目标未达 {sample_target_missed} 轮")
        if background:
            quality_notes.append(f"后台负载告警 {background} 轮")
        if sampling_bad:
            quality_notes.append(f"采样异常 {sampling_bad} 轮")
        if cpu_unavailable:
            quality_notes.append(f"CPU 峰值不可用 {cpu_unavailable} 轮")
    execution = execution_records(successful)
    row = [case_label(case, execution),latencies["p50"],throughput,rss,vram,cpu,gpu]
    if (status or len(records) != rounds or failures or hard_timeouts
            or counts["errors"] or counts["timeout"] or not accounting_complete):
        row[1:] = ["-"]*(len(HEADERS)-1)
    aggregate = dict(schema_version=5,case_key=case,experiment_fingerprint=experiment,
        model=case["model"],requested_device=case["device"],concurrency=case["budget"],
        input_tokens=case["input_tokens"],batch_size=case["batch_size"],expected_rounds=rounds,
        completed_rounds=len(records),protocol_pass_rounds=len(successful),failed_rounds=failures,
        hard_timeout_rounds=hard_timeouts,smoke_rounds=smoke_rounds,sample_target_missed_rounds=sample_target_missed,
        comparison_eligible_rounds=eligible,skipped=bool(status),skip_reason=status["reason"] if status else None,
        input_length_semantics="nominal",metrics=metrics,execution=execution,
        batch_counts=counts,batch_accounting_complete=accounting_complete,
        background_warning_rounds=background,sampling_invalid_rounds=sampling_bad,
        cpu_peak_unavailable_rounds=cpu_unavailable,quality_notes=quality_notes,
        exclusion_reasons=sorted({reason for r in records for reason in r.get("exclusion_reasons",[])}),
        rounds=[str(p.relative_to(root)) for p in paths],result_path=str(cached.relative_to(root)),
        capability_result=str(status_path.relative_to(root)) if status else None,
        signatures=signatures,table_row=row,renderer_sha256=REPORT_HASH)
    folder.mkdir(parents=True,exist_ok=True)
    save(cached,aggregate)
    return row,aggregate


def environment_intro(root, config):
    recorded = config["provenance"]["machine"]
    machine = dict(recorded)
    extra_path = root/"environment-extra.json"
    if extra_path.exists():
        extra = json.loads(extra_path.read_text())
        if extra.get("recorded_machine") == recorded:
            for key,value in extra.get("machine",{}).items():
                if machine.get(key) in (None,"",[]):
                    machine[key] = value
    model = machine.get("model") or "N/A"
    identifier = machine.get("model_identifier")
    if identifier:
        model += f"（{identifier}）"
    cpu = machine.get("cpu") or "N/A"
    physical = machine.get("physical_cpus") or "N/A"
    logical = machine.get("logical_cpus") or "N/A"
    gpus = " / ".join(machine.get("gpus") or []) or "N/A"
    ram = machine.get("ram_bytes")
    ram_text = f"{ram/1024**3:g} GiB" if isinstance(ram,(int,float)) and ram > 0 else "N/A"
    os_name = machine.get("os") or "N/A"
    title = "本地模型性能基准 v5"+("（短时验证）" if get(config,"profile","name") == "smoke" else "")
    if get(config,"profile","fixed_batch"):
        title += "（固定 native batch）"
    return (f"# {title}\n\n"
            f"机型：{model} · CPU：{cpu}（{physical} 物理核 / {logical} 逻辑核）\n\n"
            f"GPU：{gpus} · 内存：{ram_text} · 系统：{os_name}\n\n")


def generate(root):
    manifest = json.loads((root/"manifest.json").read_text())
    config = manifest["configuration"]
    if get(config,"policy","schema_version") != 5:
        raise RuntimeError("The v5 renderer requires a performance-only v5 manifest; historical results must remain unchanged")
    rows,aggregates = [],[]
    for case in config["cases"]:
        row,aggregate = summarize_case(root,case,config["rounds"],manifest["fingerprint"])
        rows.append(row)
        aggregates.append(aggregate)
    skipped = sum(c["skipped"] for c in aggregates)
    stats = dict(cases=len(rows),expected_rounds=len(rows)*config["rounds"],
        skipped_cases=skipped,skipped_rounds=skipped*config["rounds"],
        runnable_rounds=(len(rows)-skipped)*config["rounds"],
        completed_rounds=sum(c["completed_rounds"] for c in aggregates),
        complete_cases=sum(c["completed_rounds"] == c["expected_rounds"] for c in aggregates),
        protocol_pass_rounds=sum(c["protocol_pass_rounds"] for c in aggregates),
        failed_rounds=sum(c["failed_rounds"] for c in aggregates),
        hard_timeout_rounds=sum(c["hard_timeout_rounds"] for c in aggregates),
        comparison_eligible_rounds=sum(c["comparison_eligible_rounds"] for c in aggregates),
        background_warning_cases=sum(c["background_warning_rounds"] > 0 for c in aggregates),
        completed_batches=sum(c["batch_counts"]["completed"] for c in aggregates),
        failed_batches=sum(c["batch_counts"]["errors"] for c in aggregates),
        timed_out_batches=sum(c["batch_counts"]["timeout"] for c in aggregates))
    save(root/"summary-stats.json",stats)
    save(root/"summary.json",[dict(zip(HEADERS,row)) for row in rows])
    save(root/"metrics.json",aggregates)
    intro = environment_intro(root,config)
    markdown = intro+"|"+"|".join(HEADERS)+"|\n|"+"|".join(["---"]*len(HEADERS))+"|\n"
    for row,aggregate in zip(rows,aggregates):
        cells = [str(v).replace("|","\\|").replace("\n"," ") for v in row]
        cells[0] = f'[{cells[0]}]({aggregate["result_path"]})'
        markdown += "|"+"|".join(cells)+"|\n"
    (root/"REPORT.md").write_text(markdown)
    render_html(root,intro,rows,aggregates)


def render_html(root,intro,rows,aggregates):
    headers = "".join(f'<th scope="col">{html.escape(h)}</th>' for h in HEADERS)
    body = []
    for row,aggregate in zip(rows,aggregates):
        case = aggregate["case_key"]
        notes = html.escape("；".join(aggregate.get("quality_notes",[])))
        identity = (f'<a href="{html.escape(aggregate["result_path"])}" title="{notes}">{html.escape(case["model"].removeprefix("local/"))}</a>'
                    f'<span class="case-shape" title="L = 输入长度配置；B = batch size；C = CLI --embedding-concurrency；设备为请求配置">L={case["input_tokens"]} · B={case["batch_size"]} · C={html.escape(str(case["budget"]))} · {html.escape(case["device"].upper())}</span>')
        execution = aggregate.get("execution", [])
        if execution:
            providers = "/".join(sorted({r["actual_provider"] for r in execution}))
            fallback = ("（CPU fallback）" if case["device"] != "cpu"
                        and all(r["actual_provider"].lower() in ("cpu", "cpuexecutionprovider") for r in execution) else "")
            identity += (f'<span class="case-shape">{html.escape(case["device"].upper())} → '
                         f'{html.escape(providers)}{fallback}</span>'
                         f'<span class="case-shape" title="Ceff = 有效执行预算；Q = 实际准入 workers；threads = 每个 Session 的 ORT intra-op 线程数；sessions = 阶段结束时数量；native B = 测量阶段每次 native run 的平均行数；阶段耗时和运行期峰值见 JSON">'
                         f'{html.escape(execution_label(execution)).replace(" · threads=", "<br>threads=")}</span>')
        body.append('<tr><td>'+identity+'</td>'+"".join(f'<td>{html.escape(str(v))}</td>' for v in row[1:])+'</tr>')
    paragraphs = "".join(f'<p>{html.escape(p)}</p>' for p in intro.strip().split("\n\n")[1:])
    title = html.escape(intro.splitlines()[0].removeprefix("# "))
    page = '''<!doctype html><html lang="zh-CN"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>'''+title+'''</title>
<style>
body{font:14px/1.5 system-ui;margin:24px;color:#17212b}p{max-width:1200px}[hidden]{display:none!important}
#filter{font:inherit;padding:9px;width:min(500px,90%);margin-bottom:15px}
.table-wrap{overflow:auto;max-height:75vh;border:1px solid #ccd5df}table{border-collapse:separate;border-spacing:0}
th,td{border-right:1px solid #ddd;border-bottom:1px solid #ddd;padding:10px;text-align:left;vertical-align:top}
th{position:sticky;top:0;background:#eaf0f6;z-index:2;white-space:nowrap}
td{background:#fff;min-width:150px;max-width:260px;overflow-wrap:anywhere}tbody tr:nth-child(even) td{background:#f6f8fa}
th:first-child,td:first-child{position:sticky;left:0;z-index:1;white-space:nowrap;min-width:220px;max-width:none;border-right:2px solid #ccd5df}
th:first-child{z-index:3}.case-shape{display:block;margin-top:5px;font-size:12px;color:#526174}
</style></head><body><h1>'''+title+'''</h1>'''+paragraphs+f'''
<input id="filter" aria-label="筛选结果" placeholder="筛选模型、L/B/C、设备">
<div class="table-wrap"><table><thead><tr>{headers}</tr></thead><tbody>{''.join(body)}</tbody></table></div>
<script>
const rows = Array.from(document.querySelectorAll("tbody tr"));
document.querySelector("#filter").addEventListener("input",event=>{{
    const terms=event.target.value.toLowerCase().trim().split(/\\s+/).filter(Boolean);
    for(const row of rows)row.hidden=!terms.every(term=>row.textContent.toLowerCase().includes(term));
}});
</script></body></html>'''
    (root/"REPORT.html").write_text(page)


if __name__ == "__main__":
    generate(Path(sys.argv[1]))
