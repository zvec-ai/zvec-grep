"""PQ64 comparison: median QPS/RSS curves, without error bars."""
import argparse
import json
import pathlib
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib import font_manager

p=argparse.ArgumentParser()
p.add_argument("results",type=pathlib.Path)
p.add_argument("--font",type=pathlib.Path)
args=p.parse_args()
if args.font:
    font_manager.fontManager.addfont(str(args.font))
    plt.rcParams["font.family"]=font_manager.FontProperties(fname=str(args.font)).get_name()
plt.rcParams.update({"font.size":11,"axes.unicode_minus":False,"axes.spines.top":False,
                     "axes.spines.right":False,"svg.hashsalt":"diskann-pq64"})
rows=json.loads((args.results/"summary.json").read_text())
engines=[("full","DiskANN（完整向量打分）","#163B65"),("pq","DiskANN（PQ64 + 重排）","#E87722")]
fig,axes=plt.subplots(1,2,figsize=(13,6.1))
for ax,metric,title,unit in [(axes[0],"qps","查询吞吐量 · 越高越好","QPS（次/秒）"),
                            (axes[1],"rss_mib","查询峰值 RSS · 越低越好","MiB")]:
    for mode,label,color in engines:
        group=sorted((r for r in rows if r["mode"]==mode),key=lambda r:r["count"])
        assert len(group)==5 and all(r["repeats"]==6 for r in group)
        x=[r["count"]/10000 for r in group]; y=[r[metric] for r in group]
        ax.plot(x,y,"o-",color=color,linewidth=2.4,markersize=6,label=label)
        for px,value in zip(x,y):
            ax.annotate(f"{value:.2f}",(px,value),xytext=(0,11 if mode=="pq" else -20),
                        textcoords="offset points",ha="center",fontsize=10,color=color)
    ax.set_xticks([20,40,60,80,100],["20 万","40 万","60 万","80 万","100 万"])
    ax.set_xlim(12,108); ax.set_ylim(0,max(r[metric] for r in rows)*1.2)
    ax.set(title=title,ylabel=unit,xlabel="向量数量（768 维）")
    ax.grid(axis="y",alpha=.18)
fig.suptitle("DiskANN：启用 PQ64 后的 QPS 与内存\nCohere Wikipedia · FP16 存储 · 单并发 · Recall@10 ≥ 90%",fontsize=15,y=.98)
fig.legend(*axes[0].get_legend_handles_labels(),loc="lower center",bbox_to_anchor=(.5,.09),ncol=2,frameon=False)
fig.text(.5,.07,"每点为 6 次独立进程的中位数；两组复用相同图索引，在本轮重新交替测量。",ha="center",fontsize=10)
fig.text(.5,.035,"PQ64：每向量 64 字节编码常驻内存，图遍历用 PQ 距离，128 个候选用完整 FP16 向量重排。",ha="center",fontsize=10)
fig.text(.5,.006,"Apple M3 Pro / 36 GiB / macOS；文件缓存及其他系统负载未隔离。",ha="center",fontsize=10,color="#555555")
fig.subplots_adjust(left=.07,right=.985,top=.79,bottom=.27,wspace=.24)
fig.savefig(args.results/"qps-memory.png",dpi=200)
fig.savefig(args.results/"qps-memory.svg",metadata={"Date":None})
plt.close(fig)
svg=args.results/"qps-memory.svg"
svg.write_text("\n".join(line.rstrip() for line in svg.read_text().splitlines())+"\n")
