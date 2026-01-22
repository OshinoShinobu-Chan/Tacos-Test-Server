#!/bin/bash
set -euxo pipefail

tmp_dir=$1
submit_file=$(echo $2 | sed 's/.*\///g')
lab_name=$3
docker_image="crimmypeng/tacos:rust-1.92"

# Run the Docker container to grade the submission
# Limitations: 4GB memory, 16 CPU cores, 10GB tmpfs
docker run --name "$submit_file" \
    --rm -t -v "$tmp_dir":/workspace \
    -m 4g --cpus=16 --tmpfs /testspce:size=10g \
    "$docker_image" bash -c "
    set -euxo pipefail;
    cp -r /workspace/Tacos /testspace;
    rm -rf /testspace/src;
    tar xf /workspace/$submit_file --directory="/testspace"
    cd /testspace/tool;
    cargo grade -b ${lab_name}
"