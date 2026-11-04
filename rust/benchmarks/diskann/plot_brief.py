"""Plot the brief report, using the alternating retest for both engines at 800k."""
import argparse
import json
import pathlib

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib import font_manager, ticker

parser = argparse.ArgumentParser()
parser.add_argument("results", type=pathlib.Path, help="Directory containing million/ and retest800k/")
parser.add_argument("--font", type=pathlib.Path)
args = parser.parse_args()
if args.font:
    font_manager.fontManager.addfont(str(args.font))
    plt.rcParams["font.family"] = font_manager.FontProperties(fname=str(args.font)).get_name()
plt.rcParams.update({"font.size": 11, "axes.unicode_minus": False,
                     "axes.spines.top": False, "axes.spines.right": False,
                     "svg.hashsalt": "diskann-brief"})
original = json.loads((args.results / "million/summary.json").read_text())
retest = json.loads((args.results / "retest800k/summary.json").read_text())
rows = [dict(r, source="million") for r in original if r["count"] != 800000]
rows += [dict(r, source="retest800k/alternating") for r in retest if r["protocol"] == "alternating"]
rows.sort(key=lambda r: (r["count"], r["engine"]))
counts = [200000, 400000, 600000, 800000, 1000000]
assert len(rows) == 10
assert all(r["repeats"] == 6 and r["recall_at_10"] >= .9 for r in rows)
output = args.results / "brief"
output.mkdir(parents=True, exist_ok=True)
(output / "summary.json").write_text(json.dumps(rows, indent=2) + "\n")

engines = [("hnsw", "HNSW", "#163B65"), ("diskann", "DiskANN", "#E87722")]
fig, axes = plt.subplots(1, 2, figsize=(13, 6.4))
for ax, metric, title, unit in [
    (axes[0], "qps", "查询吞吐量 · 越高越好", "QPS（次/秒，对数刻度）"),
    (axes[1], "rss_mib", "查询峰值 RSS · 越低越好", "MiB"),
]:
    for engine, label, color in engines:
        group = [r for r in rows if r["engine"] == engine]
        assert [r["count"] for r in group] == counts
        x = [r["count"] / 10000 for r in group]
        y = [r[metric] for r in group]
        ax.plot(x, y, "o-", color=color, linewidth=2.4, markersize=6, label=label, zorder=3)
        for px, value in zip(x, y):
            offset = -22 if metric == "qps" and engine == "diskann" else 11
            ax.annotate(f"{value:.2f}", (px, value), xytext=(0, offset),
                        textcoords="offset points", ha="center", fontsize=10, color=color)
    ax.set_xticks([n / 10000 for n in counts], ["20 万", "40 万", "60 万", "80 万", "100 万"])
    ax.set_xlim(12, 108)
    ax.set(title=title, ylabel=unit, xlabel="向量数量（768 维）")
    ax.grid(axis="y", alpha=.18)
axes[0].set_yscale("log")
axes[0].set_ylim(30, 3200)
axes[0].set_yticks([50, 100, 200, 500, 1000, 2000])
axes[0].yaxis.set_major_formatter(ticker.StrMethodFormatter("{x:g}"))
axes[0].minorticks_off()
axes[1].set_ylim(0, 2150)
fig.suptitle("HNSW 与 DiskANN：QPS 和内存随数据规模的变化\n"
             "Cohere Wikipedia · 双 FP16 · 单并发 · Recall@10 ≥ 90%", fontsize=15, y=.98)
fig.legend(*axes[0].get_legend_handles_labels(), loc="lower center",
           bbox_to_anchor=(.5, .105), ncol=2, frameon=False)
fig.text(.5, .078, "曲线及数字为 6 次独立进程的中位数。",
         ha="center", fontsize=10)
fig.text(.5, .045, "80 万条使用两引擎交替复测结果；其余规模沿用已完成测试。各规模参数由独立验证集选择。",
         ha="center", fontsize=10)
fig.text(.5, .012, "Apple M3 Pro / 36 GiB / macOS；未固定文件缓存或隔离系统负载。",
         ha="center", fontsize=10, color="#555555")
fig.subplots_adjust(left=.075, right=.985, top=.80, bottom=.28, wspace=.25)
fig.savefig(output / "qps-memory.png", dpi=200)
fig.savefig(output / "qps-memory.svg", metadata={"Date": None})
plt.close(fig)
svg = output / "qps-memory.svg"
svg.write_text("\n".join(line.rstrip() for line in svg.read_text().splitlines()) + "\n")
