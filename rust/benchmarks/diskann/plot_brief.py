"""Plot the freshly measured zvec native DiskANN/HNSW results, without error bars."""
import argparse
import json
import pathlib

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib import font_manager, ticker

parser = argparse.ArgumentParser()
parser.add_argument("results", type=pathlib.Path, help="Directory containing native/summary.json")
parser.add_argument("--dataset", default="native-retest", choices=["native", "native-retest"])
parser.add_argument("--font", type=pathlib.Path)
args = parser.parse_args()
if args.font:
    font_manager.fontManager.addfont(str(args.font))
    plt.rcParams["font.family"] = font_manager.FontProperties(fname=str(args.font)).get_name()
plt.rcParams.update({"font.size": 11, "axes.unicode_minus": False,
                     "axes.spines.top": False, "axes.spines.right": False,
                     "svg.hashsalt": "diskann-brief"})
rows = json.loads((args.results / args.dataset / "summary.json").read_text())
config = json.loads((args.results / args.dataset / "configuration.json").read_text())
assert config["local_source_build"]
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
    (axes[0], "qps", "查询吞吐量 · 越高越好", "QPS（次/秒）"),
    (axes[1], "rss_mib", "查询峰值 RSS · 越低越好", "MiB"),
]:
    for engine, label, color in engines:
        group = [r for r in rows if r["engine"] == engine]
        assert [r["count"] for r in group] == counts
        x = [r["count"] / 10000 for r in group]
        y = [r[metric] for r in group]
        ax.plot(x, y, "o-", color=color, linewidth=2.4, markersize=6, label=label, zorder=3)
        for px, value in zip(x, y):
            offset = -22 if engine == "diskann" and value > max(r[metric] for r in rows) * .12 else 11
            dx = 14 if engine == "hnsw" and metric == "qps" and px == 40 else 0
            ax.annotate(f"{value:.2f}", (px, value), xytext=(dx, offset),
                        textcoords="offset points", ha="center", fontsize=10, color=color)
    ax.set_xticks([n / 10000 for n in counts], ["20 万", "40 万", "60 万", "80 万", "100 万"])
    ax.set_xlim(12, 108)
    ax.set(title=title, ylabel=unit, xlabel="向量数量（768 维）")
    ax.grid(axis="y", alpha=.18)
axes[0].set_ylim(0, max(r["qps"] for r in rows) * 1.23)
axes[0].yaxis.set_major_formatter(ticker.StrMethodFormatter("{x:g}"))
axes[0].minorticks_off()
axes[1].set_ylim(0, max(r["rss_mib"] for r in rows) * 1.23)
fig.suptitle("zvec HNSW 与 DiskANN：QPS 和内存随数据规模的变化\n"
             "Cohere Wikipedia · 双 FP16 · 单并发 · Recall@10 ≥ 90%", fontsize=15, y=.98)
fig.legend(*axes[0].get_legend_handles_labels(), loc="lower center",
           bbox_to_anchor=(.5, .105), ncol=2, frameon=False)
note = ("完整查询集预热 2 遍，再计时 5 遍；曲线及数字为 6 次独立进程的中位数。"
        if args.dataset == "native-retest" else
        "曲线及数字为 6 次独立进程的中位数；QPS 有明显波动，范围见报告。")
fig.text(.5, .078, note,
         ha="center", fontsize=10)
efforts = {engine: " / ".join(str(r["effort"]) for r in rows if r["engine"] == engine)
           for engine in ["hnsw", "diskann"]}
fig.text(.5, .045, f"各规模 HNSW ef：{efforts['hnsw']}；DiskANN L：{efforts['diskann']}，PQ64。",
         ha="center", fontsize=10)
fig.text(.5, .012, f"本地 zvec {config['zvec_source_commit'][:8]} · Apple M3 Pro / 36 GiB / macOS；其他应用正常运行，未清空系统文件缓存。",
         ha="center", fontsize=10, color="#555555")
fig.subplots_adjust(left=.075, right=.985, top=.80, bottom=.28, wspace=.25)
fig.savefig(output / "qps-memory.png", dpi=200)
fig.savefig(output / "qps-memory.svg", metadata={"Date": None})
plt.close(fig)
svg = output / "qps-memory.svg"
svg.write_text("\n".join(line.rstrip() for line in svg.read_text().splitlines()) + "\n")
