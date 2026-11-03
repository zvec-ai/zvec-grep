"""Render the two-engine report charts from run.py's summary.json."""
import argparse
import json
import pathlib
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib import font_manager

parser = argparse.ArgumentParser()
parser.add_argument("summary", type=pathlib.Path)
parser.add_argument("output", type=pathlib.Path)
parser.add_argument("--font", type=pathlib.Path)
parser.add_argument("--dataset", default="合成随机向量")
parser.add_argument("--recall-target", type=float)
parser.add_argument("--xscale", choices=["log", "linear"], default="log")
args = parser.parse_args()
if args.font:
    font_manager.fontManager.addfont(str(args.font))
    plt.rcParams["font.family"] = font_manager.FontProperties(fname=str(args.font)).get_name()
plt.rcParams.update({"font.size": 11, "axes.unicode_minus": False,
                     "axes.spines.top": False, "axes.spines.right": False})
rows = json.loads(args.summary.read_text())
assert {r["engine"] for r in rows} == {"hnsw", "diskann"}
args.output.mkdir(parents=True, exist_ok=True)
engines = [("hnsw", "HNSW FP16", "#163B65"), ("diskann", "DiskANN", "#E87722")]
efforts = "/".join(map(str, sorted({r["list_size"] for r in rows if r["engine"] == "diskann"})))
parameters = (f"参数由独立验证集选择；测试集目标 Recall@10 ≥ {args.recall_target * 100:g}%"
              if args.recall_target is not None else f"DiskANN L={efforts}")
repeats = {r["repeats"] for r in rows}
assert len(repeats) == 1
repeats = repeats.pop()

def panel(ax, metric, title, unit, log=False):
    for engine, label, color in engines:
        group = sorted((r for r in rows if r["engine"] == engine), key=lambda r: r["count"])
        scale = 100 if metric == "recall_at_10" else 1
        values = [r[metric] * scale for r in group]
        errors = [[(r[metric] - r[metric + "_min"]) * scale for r in group],
                  [(r[metric + "_max"] - r[metric]) * scale for r in group]]
        ax.errorbar([r["count"] for r in group], values, yerr=errors, fmt="o-", color=color,
                    label=label, linewidth=2, capsize=3, markersize=5)
    ax.set_xscale(args.xscale)
    counts = sorted({r["count"] for r in rows})
    labels = [f"{n/10000:g}万" if n >= 10000 else f"{n/1000:g}千" for n in counts]
    ax.set_xticks(counts, labels)
    if log:
        ax.set_yscale("log")
    ax.minorticks_off()
    ax.set(title=title, ylabel=unit, xlabel="向量数量（768 维）")
    ax.grid(axis="y", alpha=.2)

fig, axes = plt.subplots(1, 3, figsize=(15, 5.2), layout="constrained")
panel(axes[0], "qps", "查询吞吐量（越高越好）", "QPS，次/秒（对数刻度）", True)
panel(axes[1], "p95_ms", "P95 延迟（越低越好）", "毫秒（对数刻度）", True)
panel(axes[2], "recall_at_10", "近邻召回率", "Recall@10（%）")
axes[2].set_ylim(max(0, min(90, min(r["recall_at_10"] for r in rows) * 100 - 2)), 100.5)
if args.recall_target is not None:
    axes[2].axhline(args.recall_target * 100, color="#777777", linestyle="--", linewidth=1)
    axes[2].set_ylim(min(88, min(r["recall_at_10"] for r in rows) * 100 - 2), 100.5)
fig.suptitle(f"HNSW 与 DiskANN 查询性能 · {args.dataset} · 并发度 1\n"
             f"{repeats} 次独立进程中位数，误差线为最小至最大值；{parameters}", fontsize=13)
fig.legend(*axes[0].get_legend_handles_labels(), loc="outside lower center", ncol=2, frameon=False)
fig.savefig(args.output / "performance.png", dpi=180)
fig.savefig(args.output / "performance.svg")
plt.close(fig)
fig, ax = plt.subplots(figsize=(9, 5.2), layout="constrained")
panel(ax, "rss_mib", "查询峰值 RSS", "MiB")
ax.set_ylim(bottom=0)
fig.suptitle(f"HNSW 与 DiskANN 查询内存 · {args.dataset} · macOS\n"
             f"{repeats} 次独立进程峰值的中位数，误差线为最小至最大值\n{parameters}", fontsize=13)
fig.legend(*ax.get_legend_handles_labels(), loc="outside lower center", ncol=2, frameon=False)
fig.savefig(args.output / "memory.png", dpi=180)
fig.savefig(args.output / "memory.svg")
for name in ["performance.svg", "memory.svg"]:
    path = args.output / name
    path.write_text("\n".join(line.rstrip() for line in path.read_text().splitlines()) + "\n")
