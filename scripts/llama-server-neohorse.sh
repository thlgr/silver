#!/bin/bash
BACKEND=/home/u/.lmstudio/extensions/backends/llama.cpp-linux-x86_64-nvidia-cuda-avx2-2.41.0
VENDOR=/home/u/.lmstudio/extensions/backends/vendor/linux-llama-cuda-vendor-v1
export LD_LIBRARY_PATH="$BACKEND:$VENDOR"
exec "$BACKEND/llama-server" \
  --model /home/u/.lmstudio/models/TokenRhythm/NeoHorse-1-4B-GGUF/NeoHorse-1-4B-Q4_K_M.gguf \
  --alias NeoHorse-1-4B \
  --host 127.0.0.1 --port 8080 \
  --ctx-size 32768 \
  --n-gpu-layers 99 \
  --jinja \
  --parallel 1
