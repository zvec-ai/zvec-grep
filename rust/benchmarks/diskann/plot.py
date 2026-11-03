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

def panel(ax, metric, title, unit, log=False):
    for engine, label, color in engines:
        group = sorted((r for r in rows if r["engine"] == engine), key=lambda r: r["count"])
        scale = 100 if metric == "recall_at_10" else 1
        values = [r[metric] * scale for r in group]
        errors = [[(r[metric] - r[metric + "_min"]) * scale for r in group],
                  [(r[metric + "_max"] - r[metric]) * scale for r in group]]
        ax.errorbar([r["count"] for r in group], values, yerr=errors, fmt="o-", color=color,
                    label=label, linewidth=2, capsize=3, markersize=5)
    ax.set_xscale("log")
    ax.set_xticks([2000, 10000, 20000, 50000, 100000], ["2千", "1万", "2万", "5万", "10万"])
    if log:
        ax.set_yscale("log")
    ax.minorticks_off()
    ax.set(title=title, ylabel=unit, xlabel="向量数量（768 维）")
    ax.grid(axis="y", alpha=.2)

fig, axes = plt.subplots(1, 3, figsize=(15, 5.2), layout="constrained")
panel(axes[0], "qps", "查询吞吐量（越高越好）", "QPS，次/秒（对数刻度）", True)
panel(axes[1], "p95_ms", "P95 延迟（越低越好）", "毫秒（对数刻度）", True)
panel(axes[2], "recall_at_10", "查询质量", "Recall@10（%）")
axes[2].set_ylim(50, 102)
fig.suptitle("HNSW 与 DiskANN 查询性能 · 并发度 1\n"
             "三次独立进程中位数，误差线为最小至最大值；DiskANN L=512（≤1万）/768（>1万）", fontsize=13)
fig.legend(*axes[0].get_legend_handles_labels(), loc="outside lower center", ncol=2, frameon=False)
fig.savefig(args.output / "performance.png", dpi=180)
fig.savefig(args.output / "performance.svg")
plt.close(fig)
fig, axes = plt.subplots(1, 2, figsize=(11, 5.2), layout="constrained")
panel(axes[0], "rss_mib", "查询峰值 RSS", "MiB")
panel(axes[1], "footprint_mib", "查询峰值 footprint", "MiB")
axes[0].set_ylim(bottom=0)
axes[1].set_ylim(bottom=0)
fig.suptitle("HNSW 与 DiskANN 查询内存 · macOS\n"
             "三次独立进程峰值的中位数，误差线为最小至最大值；DiskANN L=512（≤1万）/768（>1万）", fontsize=13)
fig.legend(*axes[0].get_legend_handles_labels(), loc="outside lower center", ncol=2, frameon=False)
fig.savefig(args.output / "memory.png", dpi=180)
fig.savefig(args.output / "memory.svg")
for name in ["performance.svg", "memory.svg"]:
    path = args.output / name
    path.write_text("\n".join(line.rstrip() for line in path.read_text().splitlines()) + "\n")
