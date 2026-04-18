# Benchmark Results

Run: 2026-04-18

## Instance

- **Type**: c6in.4xlarge
- **CPU**: Intel (x86_64, 16 vCPUs)
- **Memory**: 32.0 GiB
- **Network**: Up to 50 Gigabit
- **Region**: us-east-1

Corpus: 42 images, 55 total tags

## Tool Versions

- **ocync**: 0.1.0
- **dregsy**: 0.5.2
- **regsync**: v0.11.3

## Cold sync

| Metric | ocync | dregsy | regsync |
|---|---:|---:|---:|
| Wall clock | **4m 33s** | 36m 22s | 16m 6s |
| Peak RSS | **58.1 MB** | 304.5 MB | 27.0 MB |
| Requests | **4,131** | 11,190 | 7,719 |
| Response bytes | **16.9 GB** | 43.4 GB | 35.4 GB |
| Source blob GETs | **726** | 1,324 | 1,381 |
| Source blob bytes | **77.2 KB** | 120.1 KB | 160.5 KB |
| Mounts (success/attempt) | **362/379** | 293/293 | 0/1,380 |
| Duplicate blob GETs | **0** | 1 | 1 |
| Rate-limit 429s | **0** | 49 | 0 |
