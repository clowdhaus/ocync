# Benchmark Results

Run: 2026-04-18-031724

## Instance

- **Type**: c6in.4xlarge
- **CPU**: Intel (x86_64, 16 vCPUs)
- **Memory**: 32.0 GiB
- **Network**: Up to 50 Gigabit
- **Region**: us-east-1

Corpus: 42 images, 55 total tags

## Tool Versions

- **ocync**: ocync 0.1.0
FIPS 140-3: compiled=no

## Cold sync (median)

| Metric | ocync |
|---|---:|
| Wall clock | 4m 33s |
| Peak RSS | 58.1 MB |
| Requests | 4131 |
| Response bytes | 16.9 GB |
| Source blob GETs | 726 |
| Source blob bytes | 77.2 KB |
| Mounts (success/attempt) | 362/379 |
| Duplicate blob GETs | 0 |
| Rate-limit 429s | 0 |

