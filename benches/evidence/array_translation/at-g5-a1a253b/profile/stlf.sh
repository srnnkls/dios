cd ~/build/dios/target/array-translation
p=$PWD/at-g56-stlf-a1a253b-$(date +%s); mkdir -p $p
OPS=134217728
st() { tag=$1; dir=$2; lane=$3; arm=$4; bin=$(ls $dir/mmap_workloads-* | head -1)
  taskset -c 0 perf stat -x, -e cycles,ls_bad_status2.stli_other,ls_stlf -o $p/$tag.stat -- $bin sample $dir/input $lane $arm 3 plain $p/$tag.json >/dev/null 2>&1
  awk -F, -v t="$tag" -v n=$OPS '{v[$3]=$1} END {printf "%-22s cyc/read %.2f stli_other/read %.3f stlf_hits/read %.3f\n", t, v["cycles"]/n, v["ls_bad_status2.stli_other"]/n, v["ls_stlf"]/n}' $p/$tag.stat; }
st g5-32-dios_copy at-g5/layout-32 resident_access_copy dios_copy
st g5-32-mmap_copy at-g5/layout-32 resident_access_copy mmap_copy
st g6-32-optimistic_floor at-g6/layout-32 resident_access_copy_floor optimistic_floor
bin=$(ls at-g5/layout-32/mmap_workloads-* | head -1)
echo "== consume"; objdump -d --no-show-raw-insn -C $bin | awk '/<mmap_workloads::mmap_workloads::catalog::consume>:/{f=1} f{print} f&&/ret/{exit}' | head -40
echo "OUT=$p"
