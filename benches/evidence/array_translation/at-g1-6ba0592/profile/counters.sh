cd ~/build/dios/target/array-translation
p=$PWD/at-g1-bm-6ba0592-$(date +%s); mkdir -p $p
st() { tag=$1; dir=$2; arm=$3; bin=$(ls $dir/mmap_workloads-* | head -1)
  taskset -c 0 perf stat -x, -e cycles,instructions,branches,branch-misses -o $p/$tag.stat -- $bin sample $dir/input resident_access_session_projection $arm 3 plain $p/$tag.json >/dev/null 2>&1
  awk -F, -v t="$tag" '{v[$3]=$1} END {printf "%-26s cyc/read %.2f ins/read %.2f br/read %.2f miss/1k-reads %.2f\n", t, v["cycles"]/134283264, v["instructions"]/134283264, v["branches"]/134283264, v["branch-misses"]/134283.264}' $p/$tag.stat; }
for l in 16 32 64; do st new-$l-session at-g1/layout-$l dios_session; st new-$l-mmap at-g1/layout-$l mmap_random; done
for l in 16 32 64; do st fdada37-$l-session at-g1-fdada37/layout-$l dios_session; st 377a0ad-$l-session at-g1-377a0ad/layout-$l dios_session; done
echo "== mispredicts by instruction (new layout-64, session arm)"
bin=$(ls at-g1/layout-64/mmap_workloads-* | head -1)
taskset -c 0 perf record -q -e branch-misses -c 2003 -o $p/bm.data -- $bin sample at-g1/layout-64/input resident_access_session_projection dios_session 3 plain $p/bm.json >/dev/null 2>&1
perf report -i $p/bm.data --stdio --no-children --sort symbol 2>/dev/null | grep -F "[.]" | head -5 | cut -c1-110
perf annotate -i $p/bm.data --stdio --no-source -s 'mmap_workloads::mmap_workloads::fair::dios_session_loop' 2>/dev/null | awk '/^ +[0-9]+\.[0-9]+ +: +[0-9a-f]+:/ { ins=$0; sub(/^ +[0-9.]+ +: +/, "", ins); if ($1+0 >= 4) printf "%5.1f | %-52s || prev: %s\n", $1+0, substr(ins,1,52), substr(prev,1,44); prev=ins }' | sort -rn | head -8
echo "OUT=$p"
