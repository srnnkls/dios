M = (1 << 64) - 1

def mix(w):
    w = ((w ^ (w >> 30)) * 0xbf58476d1ce4e5b9) & M
    w = ((w ^ (w >> 27)) * 0x94d049bb133111eb) & M
    return w ^ (w >> 31)

def hash_current(driver, slot, gen, granule):
    h = mix(0 ^ driver)
    h = mix(h ^ slot)
    h = mix(h ^ gen)
    return mix(h ^ granule)

def hash_packed(driver, slot, gen, granule):
    return mix(driver ^ ((slot << 32) | ((gen ^ granule) & 0xFFFFFFFF)))

DRIVER = 0x00D105EED0000001

def keys_for(files, count, pattern):
    keys, per_file = [], count // files
    for f in range(files):
        gen = 2 * f + 1
        if pattern == 'seq':
            for g in range(per_file):
                keys.append((f, gen, g))
        else:
            block, g = 0, 0
            while len([k for k in keys if k[0] == f]) < per_file:
                base = block * 64 * files + f * 64
                for i in range(64):
                    if g >= per_file: break
                    keys.append((f, gen, base + i)); g += 1
                block += 1
    return keys[:count]

def probe_stats(hasher, keys, slots):
    mask = slots - 1
    table = [None] * slots
    for k in keys:
        s = hasher(DRIVER, *k) & mask
        while table[s] is not None:
            s = (s + 1) & mask
        table[s] = k
    lens = []
    for k in keys:
        s, n = hasher(DRIVER, *k) & mask, 1
        while table[s] != k:
            s = (s + 1) & mask; n += 1
        lens.append(n)
    lens.sort()
    return (sum(lens) / len(lens), lens[int(0.99 * len(lens)) - 1], lens[-1])

print(f"{'config':<34}{'current mean/p99/max':>24}{'packed mean/p99/max':>24}  verdict")
worst = []
for slots in (1024, 131072, 524288):
    for files in (1, 16, 256):
        for pattern in ('seq', 'interleaved'):
            count = slots // 2
            if files > count: continue
            keys = keys_for(files, count, pattern)
            c = probe_stats(hash_current, keys, slots)
            p = probe_stats(hash_packed, keys, slots)
            ok = p[0] <= c[0] * 1.05 and p[1] <= c[1] * 1.05 and p[2] < 64
            worst.append(ok)
            print(f"slots={slots:<7} files={files:<4} {pattern:<12}"
                  f"{c[0]:>8.3f}/{c[1]:>3}/{c[2]:>4}    {p[0]:>8.3f}/{p[1]:>3}/{p[2]:>4}    {'PASS' if ok else 'FAIL'}")
print("ADMISSIBLE" if all(worst) else "REJECTED", "per table_tightening.md bounds")

PHI = 0x9E3779B97F4A7C15
def hash_fullwidth(driver, slot, gen, granule):
    return mix(driver ^ ((gen << 32) | granule) ^ ((slot * PHI) & M))

print()
print(f"{'config':<34}{'current mean/p99/max':>24}{'fullwidth mean/p99/max':>24}  verdict")
worst2 = []
for slots in (1024, 131072, 524288):
    for files in (1, 16, 256):
        for pattern in ('seq', 'interleaved'):
            count = slots // 2
            if files > count: continue
            keys = keys_for(files, count, pattern)
            c = probe_stats(hash_current, keys, slots)
            p = probe_stats(hash_fullwidth, keys, slots)
            ok = p[0] <= c[0] * 1.05 and p[1] <= c[1] * 1.05 and p[2] < 64
            worst2.append(ok)
            print(f"slots={slots:<7} files={files:<4} {pattern:<12}"
                  f"{c[0]:>8.3f}/{c[1]:>3}/{c[2]:>4}    {p[0]:>8.3f}/{p[1]:>3}/{p[2]:>4}    {'PASS' if ok else 'FAIL'}")
print("ADMISSIBLE" if all(worst2) else "REJECTED", "— fullwidth candidate per the same bounds")
