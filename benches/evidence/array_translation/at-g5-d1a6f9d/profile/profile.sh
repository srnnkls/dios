cd ~/build/dios/target/array-translation
p=$PWD/at-g56-profile-d1a6f9d-$(date +%s); mkdir -p $p
OPS=134217728
st() { tag=$1; dir=$2; lane=$3; arm=$4; bin=$(ls $dir/mmap_workloads-* | head -1)
  taskset -c 0 perf stat -x, -e cycles,instructions,branches,branch-misses,L1-dcache-load-misses -o $p/$tag.stat -- $bin sample $dir/input $lane $arm 3 plain $p/$tag.json >/dev/null 2>&1
  awk -F, -v t="$tag" -v n=$OPS '{v[$3]=$1} END {printf "%-22s cyc/read %.2f ins/read %.2f br/read %.2f miss/1k %.2f l1miss/read %.3f\n", t, v["cycles"]/n, v["instructions"]/n, v["branches"]/n, v["branch-misses"]*1000/n, v["L1-dcache-load-misses"]/n}' $p/$tag.stat; }
st g5-32-dios_copy at-g5/layout-32 resident_access_copy dios_copy
st g5-32-mmap_copy at-g5/layout-32 resident_access_copy mmap_copy
st g6-32-optimistic_floor at-g6/layout-32 resident_access_copy_floor optimistic_floor
st g5-32-plain_array at-g5/layout-32 resident_access_copy plain_array
echo "== cycles by symbol (dios_copy, layout 32)"
bin=$(ls at-g5/layout-32/mmap_workloads-* | head -1)
taskset -c 0 perf record -q -e cycles -c 200003 -o $p/cyc.data -- $bin sample at-g5/layout-32/input resident_access_copy dios_copy 3 plain $p/cyc.json >/dev/null 2>&1
perf report -i $p/cyc.data --stdio --no-children --sort symbol 2>/dev/null | grep -F "[.]" | head -6 | cut -c1-140
top=$(perf report -i $p/cyc.data --stdio --no-children --sort symbol 2>/dev/null | grep -F "[.]" | head -1 | sed 's/.*\[\.\] //' | sed 's/ *$//')
echo "== hottest instructions in: $top"
perf annotate -i $p/cyc.data --stdio --no-source -s "$top" 2>/dev/null | awk '/^ +[0-9]+\.[0-9]+ +: +[0-9a-f]+:/ { ins=$0; sub(/^ +[0-9.]+ +: +/, "", ins); if ($1+0 >= 2) printf "%5.1f | %s\n", $1+0, substr(ins,1,70) }' | sort -rn | head -14
echo "== instruction count of $top"
perf annotate -i $p/cyc.data --stdio --no-source -s "$top" 2>/dev/null | grep -cE '^ +[0-9.]+ +: +[0-9a-f]+:'
echo "OUT=$p"
