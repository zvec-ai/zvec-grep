"""Audit raw single-pass results, including both recalls, before publishing."""
import argparse
import gzip
import hashlib
import json
import re
import shutil
import statistics
import subprocess
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent
COUNTS = [600000, 800000, 1000000]


def sha(path):
    with Path(path).open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def save(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + '\n')


def near(a, b):
    assert abs(a - b) < 1e-8 * max(1, abs(a), abs(b)), (a, b)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--base', type=Path, required=True)
    args = parser.parse_args()
    base = args.base
    dest = HERE / 'results/recall-parity'
    dest.mkdir(exist_ok=True)
    cfg = json.loads((base / 'measure/configuration.json').read_text())
    source = Path(cfg['source'])
    assert sha(HERE / 'run_recall_comparison.py') == cfg['runner_sha256']
    assert sha(HERE / 'harness/src/bin/memory-bench.rs') == cfg['harness_sha256']
    for name, digest in cfg['source_sha256'].items():
        assert sha(source / name) == digest
        committed = subprocess.check_output(['git', 'show', f"{cfg['source_commit']}:{name}"], cwd=source)
        assert hashlib.sha256(committed).hexdigest() == digest
    for name, digest in cfg['microsoft_source_sha256'].items():
        assert sha(HERE.parents[2] / name) == digest
    for kind in ['libraries', 'binaries']:
        for info in cfg[kind].values():
            assert sha(info['path']) == info['sha256']

    data = Path(cfg['data'])
    holdout = Path('/tmp/zvec-unseen-opt-20261105/holdout')
    dm = json.loads((data / 'manifest.json').read_text())
    hm = json.loads((holdout / 'manifest.json').read_text())
    assert sha(data / 'manifest.json') == cfg['data_manifest_sha256']
    assert sha(holdout / 'manifest.json') == cfg['holdout_manifest_sha256']
    assert sha('/tmp/zvec-cohere-million/data/base.f32') == dm['base_sha256'] == hm['base_sha256']
    for folder, manifest in [(data, dm), (holdout, hm)]:
        for name, info in manifest['files'].items():
            assert sha(folder / name) == info['sha256']
    source_ids, fp32, fp16 = set(), set(), set()
    fingerprints = {}
    sets = [(name, data / f'{name}.f32', ids) for name, ids in dm['query_source_row_ids'].items()]
    sets.append(('test-6', holdout / 'queries.f32', hm['query_source_row_ids']))
    for name, path, ids in sets:
        assert len(ids) == len(set(ids)) and not source_ids.intersection(ids)
        source_ids.update(ids)
        vectors = np.fromfile(path, dtype='<f4').reshape(-1, 768)
        assert len(vectors) == len(ids)
        fingerprints[name] = []
        for vector in vectors:
            f = hashlib.sha256(vector.tobytes()).hexdigest()
            h = hashlib.sha256(vector.astype('<f2').tobytes()).hexdigest()
            assert f not in fp32 and h not in fp16
            fp32.add(f)
            fp16.add(h)
            fingerprints[name].append(f)
    assert len(source_ids) == len(fp32) == len(fp16) == 4500

    evidence, truths, all_rows = [], {}, {}
    for stage in ['validation-matched-recall', 'validation', 'measure']:
        folder = base / stage
        conf = json.loads((folder / 'configuration.json').read_text())
        for key in ['source_sha256', 'libraries', 'binaries', 'selection', 'runner_sha256']:
            if stage == 'validation-matched-recall' and key == 'selection':
                continue
            assert conf[key] == cfg[key]
        assert json.loads((folder / 'status.json').read_text())['phase'] == 'complete'
        for name, info in json.loads((folder / 'index-inventory.json').read_text()).items():
            file = Path(name)
            assert [file.stat().st_size, file.stat().st_mtime_ns] == info
        rows = json.loads((folder / 'runs.json').read_text())
        all_rows[stage] = rows
        assert len(rows) == (42 if stage == 'measure' else 27)
        for row in rows:
            path = folder / row['raw_file']
            raw = [json.loads(line) for line in path.read_text().splitlines()]
            actual = next(r for r in raw if r['stage'] == 'queried')
            ids = next(r['ids'] for r in raw if r['stage'] == 'results')
            n, repeat = row['count'], row['repeat']
            formal = stage == 'measure'
            q = 500 if formal else 400
            gtfile = (holdout / f'truth-{n}.json' if repeat == 6 else data / f'test-{repeat}-truth-{n}.json') if formal else data / f'validation-truth-{n}.json'
            gt = json.loads(gtfile.read_text())
            truths[str(gtfile)] = gt
            assert len(ids) == len(gt) == actual['queries'] == q
            assert all(len(x) == len(set(x)) == 10 for x in ids)
            assert all(re.fullmatch(r'v\d{8}', x) and int(x[1:]) < n for group in ids for x in group)
            assert actual['warmup_queries'] == 600 and actual['test_passes'] == 1 and actual['test_warmup_passes'] == 0
            hits = sum(len(set(x) & set(y)) for x, y in zip(ids, gt))
            near(row['recall_at_10'], hits / (q * 10))
            assert row['recall_hits'] == hits
            near(row['batch_s'], actual['batch_s'])
            near(row['qps'], q / actual['batch_s'])
            assert hashlib.sha256(json.dumps(ids, separators=(',', ':')).encode()).hexdigest() == row['results_sha256']
            stderr = path.with_suffix('.stderr').read_text()
            near(row['rss_mib'], int(re.search(r'(\d+)\s+maximum resident set size', stderr).group(1)) / 2**20)
            assert row['rss_mib'] >= actual['rss_peak_after_test_mib']
            loaded = next(r['path'] for r in raw if r['stage'] == 'library')
            assert Path(loaded).resolve() == Path(cfg['libraries'][row['backend']]['path']).resolve()
            assert row['loaded_library'] == loaded
            times = sorted(actual['latencies_ms'])
            assert len(times) == q and min(times) > 0
            near(actual['p50_ms'], times[q // 2])
            near(actual['p95_ms'], times[q * 95 // 100])
            assert sum(times) / 1000 <= actual['batch_s'] * 1.001
            if formal:
                assert row['effort'] == (128 if row['backend'] == 'microsoft' else cfg['selection']['settings'][str(n)]['effort'])
                assert row['workers'] == (1 if row['backend'] == 'microsoft' else cfg['read_workers'])
            evidence.append(dict(stage=stage, path=row['raw_file'], jsonl=path.read_text(), stderr=stderr))
        for summary in json.loads((folder / 'summary.json').read_text()):
            group = [r for r in rows if r['count'] == summary['count'] and r['backend'] == summary['backend']]
            assert len(group) == summary['processes']
            near(summary['qps'], sum(r['queries'] for r in group) / sum(r['batch_s'] for r in group))
            near(summary['rss_mib'], statistics.median(r['rss_mib'] for r in group))
            near(summary['recall_at_10'], sum(r['recall_hits'] for r in group) / sum(r['queries'] * 10 for r in group))
        target = dest / stage
        target.mkdir(exist_ok=True)
        for file in folder.glob('*.json'):
            shutil.copy2(file, target / file.name)
        shutil.copy2(folder / 'source.patch', target / 'source.patch')

    formal = all_rows['measure']
    summaries = json.loads((base / 'measure/summary.json').read_text())
    def row(n, backend):
        return next(r for r in summaries if r['count'] == n and r['backend'] == backend)
    comparisons = []
    for n in COUNTS:
        for backend in ['zvec', 'microsoft']:
            group = [r for r in formal if r['count'] == n and r['backend'] == backend]
            assert sorted(r['repeat'] for r in group) == list(range(7))
            assert sum(r['queries'] for r in group) == 3500
        z, m = row(n, 'zvec'), row(n, 'microsoft')
        pairs = [r['qps'] / next(s['qps'] for s in formal if s['backend'] == 'microsoft' and s['count'] == n and s['repeat'] == r['repeat']) for r in formal if r['backend'] == 'zvec' and r['count'] == n]
        comparisons.append(dict(count=n, qps_ratio=z['qps']/m['qps'], rss_ratio=z['rss_mib']/m['rss_mib'], recall_gap=z['recall_at_10']-m['recall_at_10'], paired_wins=sum(p > 1 for p in pairs), paired_ratios=pairs))
    # Preserve every pilot and failed startup, including rejected experiments.
    exploratory = []
    for folder in sorted(base.iterdir()):
        if not folder.is_dir() or folder.name in ['validation-matched-recall', 'validation', 'measure'] or not (folder / 'configuration.json').exists() or folder.name == 'graph64':
            continue
        files = [f for f in folder.rglob('*') if f.suffix in ['.json', '.jsonl', '.stderr'] and f.is_file()]
        exploratory.append(dict(folder=folder.name, files={str(f.relative_to(folder)):f.read_text() for f in files}))
    with (dest / 'raw-evidence.json.gz').open('wb') as stream:
        with gzip.GzipFile(fileobj=stream, mode='wb', filename='', mtime=0) as gz:
            prototype_builds = []
            for folder in base.iterdir():
                if folder.is_dir() and (folder / 'build.json').exists():
                    prototype_builds.append(dict(folder=folder.name, files={name:(folder/name).read_text() for name in ['build.json', 'source.patch', 'build.log'] if (folder/name).exists()}))
            cpp_logs = {str(f.relative_to(base)):f.read_text() for f in (base/'frozen-test-runs').rglob('output.txt')}
            gz.write(json.dumps(dict(records=evidence, truth=truths, query_fingerprints=fingerprints, exploratory=exploratory, prototype_builds=prototype_builds, cpp_logs=cpp_logs), separators=(',', ':')).encode())
    for name in ['cpp-tests.json', 'selection-draft.json']:
        shutil.copy2(base / name, dest / name)
    for name in ['build.json', 'source.patch']:
        shutil.copy2(base / cfg['selection']['variant'] / name, dest / name)
    shutil.copy2(base / 'graph64/configuration.json', dest / 'graph-build-configuration.json')
    shutil.copy2(base / 'graph64/graph-build-at-build.rs', dest / 'graph-build-at-build.rs')
    assert sha(dest/'graph-build-at-build.rs') == json.loads((dest/'graph-build-configuration.json').read_text())['builder_source_sha256']
    for n in COUNTS:
        shutil.copy2(base / f'graph64/builds/diskann-{n}.json', dest / f'graph-build-{n}.json')
    matched = dest / 'validation-matched-recall'
    matched.mkdir(exist_ok=True)
    for file in (base / 'validation-matched-recall').glob('*.json'):
        shutil.copy2(file, matched / file.name)
    tests = json.loads((dest / 'cpp-tests.json').read_text())
    assert all(r['exit_code'] == 0 for r in tests.values())
    audit = dict(passed=True, formal_processes=42, validation_processes=54, final_validation_processes=27, matched_recall_validation_processes=27, query_sets_disjoint_by_source_ids_fp32_fp16=True, each_test_query_once_per_backend_scale=True, previously_used_queries=True, actual_library_verified=True, minimum_recall=min(r['recall_at_10'] for r in formal), comparisons=comparisons, cpp_tests_passed=sum(r['passed'] for r in tests.values()), cpp_tests_skipped=sum(r.get('skipped', 0) for r in tests.values()), raw_sha256=sha(dest/'raw-evidence.json.gz'))
    save(dest / 'audit.json', audit)
    publish_report(dest, cfg, summaries, audit)
    print(json.dumps(audit, indent=2))


def publish_report(dest, cfg, summaries, audit):
    # Deliberately publish failures too: the result must not depend on winning.
    def row(n, b):
        return next(r for r in summaries if r['count'] == n and r['backend'] == b)
    table = '\n'.join(f"| {n//10000} 万 | {row(n,'zvec')['qps']:.2f} | {row(n,'microsoft')['qps']:.2f} | {(row(n,'zvec')['qps']/row(n,'microsoft')['qps']-1)*100:+.2f}% | {row(n,'zvec')['rss_mib']:.2f} / {row(n,'microsoft')['rss_mib']:.2f} | {row(n,'zvec')['recall_at_10']:.2%} / {row(n,'microsoft')['recall_at_10']:.2%} |" for n in COUNTS)
    ratios = [r['qps_ratio'] for r in audit['comparisons']]
    conclusion = '三个规模的汇总 QPS 均领先微软适配层。' if min(ratios) > 1 else ('三个规模均达到事先约定的 QPS 相当门槛（不低于微软的 95%）。' if min(ratios) >= .95 else '尚未在全部规模达到事先约定的 QPS 相当门槛（不低于微软的 95%）。')
    if audit['minimum_recall'] < .90:
        conclusion += '存在召回率低于 90% 的分片，本轮未满足验收条件。'
    matched = json.loads((dest / 'validation-matched-recall/summary.json').read_text())
    matched_rows = []
    for n in COUNTS:
        z = next(r for r in matched if r['count'] == n and r['backend'] == 'zvec')
        m = next(r for r in matched if r['count'] == n and r['backend'] == 'microsoft')
        matched_rows.append(f"| {n//10000} 万 | {z['qps']:.2f} / {m['qps']:.2f} | {(z['qps']/m['qps']-1)*100:+.2f}% | {z['recall_at_10']:.2%} / {m['recall_at_10']:.2%} |")
    matched_table = '\n'.join(matched_rows)
    validation = json.loads((dest / 'validation/summary.json').read_text())
    ablation = []
    for n in COUNTS:
        old = next(r for r in validation if r['count'] == n and r['backend'] == 'baseline')
        new = next(r for r in validation if r['count'] == n and r['backend'] == 'zvec')
        ablation.append(f"| {n//10000} 万 | {old['qps']:.2f} → {new['qps']:.2f} | {(new['qps']/old['qps']-1)*100:+.2f}% | {old['recall_at_10']:.2%} → {new['recall_at_10']:.2%} |")
    ablation_table = '\n'.join(ablation)
    report = f'''# Cohere：zvec DiskANN 与微软方案对比

测试日期：2026-11-05；Apple M3 Pro、36 GiB、macOS；本地 zvec `{cfg['source_commit'][:8]}`。

在双方召回率均 ≥90% 的门槛下，{conclusion} 下表同时列出双方实际 Recall@10；不同召回率不能视为完全相同的查询质量。

## 单遍测试结果

| 向量数 | zvec DiskANN QPS | 微软 QPS | QPS 差值 | RSS：zvec / 微软（MiB） | Recall@10：zvec / 微软 |
|---|---:|---:|---:|---:|---:|
{table}

![QPS 与查询进程峰值 RSS](results/recall-parity/qps-memory.png)

QPS 为 3,500 条查询除以七个批次的总时间；RSS 为七个进程完整生命周期峰值的中位数，包含加载、预热、查询和退出。查询并发为 1。全部 42 个测试进程的最低召回率为 **{audit['minimum_recall']:.2%}**。所有分片均保留，未删除慢分片。“QPS 不低于微软的 95%”是预先设定的工程门槛，不是统计等价证明；汇总领先也不保证每条请求更快。

## 接近同召回率时仍有多少差距

以下为正式测试前、独立 400 条验证集的三轮交替对照。zvec 使用 L=144／112／136，微软保持 L=128。该组在 60 万、100 万未通过 QPS 相当门槛，完整保留；随后只用验证集选择上述正式配置。

| 向量数 | QPS：zvec / 微软 | QPS 差值 | Recall@10：zvec / 微软 |
|---|---:|---:|---:|
{matched_table}

**提高 zvec 搜索宽度、把实际召回率拉近微软后，60 万和 100 万仍有性能差距。** 正式配置满足共同的 90% 下限，但更低的搜索宽度同时减少了查询工作量和召回率，不能据此宣称同召回率下全面超过微软。[完整对照](results/recall-parity/validation-matched-recall/summary.json)。

## 本轮改变

1. **让 PQ 导航使用重建向量的余弦分数。** 旧码本按 L2 初始化，重建向量长度变化会影响候选排序。现在计算归一化点积，最终仍用 FP16 原向量精确打分；未降低 PQ64 精度。
2. **在原预算内缓存重建向量的逆范数。** 百万条约 3.81 MiB，从 8 MiB 读取缓存总预算中扣除；剩余容量用于节点记录。表太大时使用融合计算，避免不断新增常驻内存。标量页缓存仍为 1 MiB。
3. **缓存命中后提前展开节点。** 已在缓存中的邻居可及时参与候选选择，减少不必要的后续读盘。读取仍保留 F_NOCACHE；这会改变遍历顺序，需要重新验证召回。
4. **单独提高构图质量。** 本轮 zvec 出度 64、构图 L=200，原基线为 32/100；微软维持 48/100。需要重建索引才能得到本轮图质量，不能把全部收益归因于代码修改。应用默认构图参数未自动修改。

下面单独比较代码变化：同一新图、相同 L=104、24 个读取线程和 8 MiB 读取／1 MiB 标量预算，在独立验证集交替运行三轮。基线为 `6473f807`，新版本为 `{cfg['source_commit'][:8]}`。三个规模均同时提高了 QPS 和召回；这部分收益无需用降低召回率解释。

| 向量数 | QPS：基线 → 本轮 | QPS 提升 | Recall@10：基线 → 本轮 |
|---|---:|---:|---:|
{ablation_table}

读取线程为 {cfg['read_workers']}；查询 L（60/80/100 万）为 {' / '.join(str(cfg['selection']['settings'][str(n)]['effort']) for n in COUNTS)}。微软 L=128、重排 128。线程调度、额外入口采样等未验证或无稳定收益的实验没有纳入最终代码；探索数据保留在原始证据中。C++ 改动尚未进入已发布的 0.8.0 SDK，需要重新编译并链接本地库。

## 数据与测量约束

沿用同一 Cohere 基库及按各规模重新计算的精确 GT：768 维、cosine、FP16、PQ64、top-10。600 条预热、400 条验证、3,500 条测试在源行号、FP32 和 FP16 内容上互不重叠。每个实现、每个规模下，每条测试查询只运行一次；每个进程先运行独立预热集一次。测试查询在此前轮次使用过，本轮属于回归验证，不是全新盲测。

参数仅在验证集选择，随后三轮交替验证并冻结源码、库和参数，再执行七个互斥测试分片。两边均使用在验证前创建的独立 APFS 索引副本；未清空系统文件缓存，也未隔离其他桌面应用。不能把刚建完索引后的高 QPS 当作稳定收益，也不能宣称这是严格冷盘测试。

微软组是 **Microsoft Rust DiskANN 0.61.0 + 自定义 SQLite/PQ 适配层**，SQLite 页缓存约 256 KiB、mmap 上限 16 MiB，并非微软官方原生 SSD 引擎。双方图、码本及接口成本不同。

缓存预算不是进程 RSS 上限。双方 PQ64 编码仍常驻；zvec 还承担 Collection、doc ID、元数据和读取线程等成本。RSS 不包含全部系统文件缓存，不能从进程 RSS 直接推导整机内存占用或对其他应用的影响。

## 验证与复现

{audit['cpp_tests_passed']} 项 C++ 测试通过、{audit['cpp_tests_skipped']} 项跳过（详见测试记录）。覆盖 FP16/FP32 余弦重建参考值、零范数、非均分 chunk、码本导入与序列化、度量覆盖、缓存读写、线程池和 Collection 闭环。ThreadSanitizer 程序在启动阶段被终止，本轮没有可用的 TSan 结论。若干实验程序也在查询前启动失败，未产生 QPS 数据，单独保留原始记录。

每个进程在查询前检查实际加载的动态库路径，父进程核对 SHA-256；报告从实际结果 ID、精确 GT、计时和 RSS 原文独立复算。[核验记录](results/recall-parity/audit.json)、[正式汇总](results/recall-parity/measure/summary.json)、[三轮验证](results/recall-parity/validation/summary.json)、[原始证据](results/recall-parity/raw-evidence.json.gz)、[配置](results/recall-parity/measure/configuration.json)、[测试](results/recall-parity/cpp-tests.json)。

复现入口：[构建库](build_recall_variant.py)、[构建图](build_quality_graphs.py)、[验证试验](run_recall_pilot.py)、[冻结测量](run_recall_comparison.py)、[核验与绘图](publish_recall_comparison.py)。本机目录：`/tmp/zvec-recall-parity-20261105`。[上轮报告](REPORT_SCALAR_CACHE_CN.md)保留原测试结果。
'''
    (HERE / 'REPORT_NODE_CN.md').write_text(report)
    import matplotlib
    matplotlib.use('Agg')
    import matplotlib.pyplot as plt
    from matplotlib import font_manager
    font = '/System/Library/Fonts/STHeiti Light.ttc'
    font_manager.fontManager.addfont(font)
    plt.rcParams.update({'font.family':font_manager.FontProperties(fname=font).get_name(), 'font.size':12, 'axes.unicode_minus':False, 'svg.hashsalt':'recall-parity'})
    fig, axes = plt.subplots(1, 2, figsize=(13, 6.4))
    colors = {'microsoft':'#163B65', 'zvec':'#E87722'}
    for ax, key, title, unit in [(axes[0], 'qps', '查询吞吐量 · 越高越好', 'QPS（次/秒）'), (axes[1], 'rss_mib', '查询进程峰值 RSS · 越低越好', 'MiB')]:
        for backend, label, marker, style in [('microsoft','微软 Rust DiskANN + SQLite/PQ','s','--'), ('zvec','zvec DiskANN','o','-')]:
            values = [row(n, backend)[key] for n in COUNTS]
            ax.plot([60,80,100], values, label=label, color=colors[backend], marker=marker, linestyle=style, lw=2.2)
            for n, value in zip(COUNTS, values):
                ax.annotate(f'{value:.2f}', (n/10000, value), xytext=(0, 11 if backend=='zvec' else -19), textcoords='offset points', ha='center', color=colors[backend], fontsize=10)
        ax.set_title(title); ax.set_ylabel(unit); ax.set_xlabel('Cohere 向量数量（768 维）')
        ax.set_xticks([60,80,100], ['60 万','80 万','100 万']); ax.set_xlim(54,106)
        ax.set_ylim(0, max(row(n,b)[key] for n in COUNTS for b in colors)*1.25)
        ax.grid(axis='y',alpha=.2); ax.spines[['top','right']].set_visible(False)
    fig.suptitle('3,500 条互斥测试查询 · 单遍回归\nFP16 · PQ64 · 查询并发 1 · 双方实际召回率见报告', fontsize=16)
    handles, labels = axes[0].get_legend_handles_labels()
    fig.legend(handles, labels, loc='lower center', bbox_to_anchor=(.5,.105), ncol=2, frameon=False)
    fig.text(.5,.082,'每进程 600 条独立预热 + 500 条测试；每条测试查询只执行一次。',ha='center',fontsize=10)
    fig.text(.5,.047,'zvec：8 MiB 读取缓存（含范数表）+ 1 MiB 标量页缓存；双方图参数不同。',ha='center',fontsize=10)
    fig.text(.5,.015,f"本地 zvec {cfg['source_commit'][:8]} · Apple M3 Pro / 36 GiB / macOS · 2026-11-05",ha='center',fontsize=9,color='#555555')
    fig.subplots_adjust(left=.08,right=.985,top=.80,bottom=.28,wspace=.25)
    fig.savefig(dest/'qps-memory.png',dpi=200)
    fig.savefig(dest/'qps-memory.svg',metadata={'Date':None})
    plt.close(fig)
    svg = dest / 'qps-memory.svg'
    svg.write_text('\n'.join(line.rstrip() for line in svg.read_text().splitlines()) + '\n')


if __name__ == '__main__':
    main()
