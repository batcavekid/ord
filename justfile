set positional-arguments

watch +args='test':
  cargo watch --clear --exec '{{args}}'

ci: clippy forbid
  cargo fmt -- --check
  cargo test --all

forbid:
  ./bin/forbid

fmt:
  cargo fmt

clippy:
  cargo clippy --all --all-targets

deploy branch chain domain:
  ssh root@{{domain}} "mkdir -p deploy \
    && apt-get update --yes \
    && apt-get upgrade --yes \
    && apt-get install --yes git rsync"
  rsync -avz deploy/checkout root@{{domain}}:deploy/checkout
  ssh root@{{domain}} 'cd deploy && ./checkout {{branch}} {{chain}} {{domain}}'

deploy-mainnet: (deploy "master" "main" "ordinals.com")

deploy-signet branch="master": (deploy branch "signet" "signet.ordinals.com")

log unit="ord" domain="ordinals.com":
  ssh root@{{domain}} 'journalctl -fu {{unit}}'

test-deploy:
  ssh-keygen -f ~/.ssh/known_hosts -R 192.168.56.4
  vagrant up
  ssh-keyscan 192.168.56.4 >> ~/.ssh/known_hosts
  rsync -avz \
    --delete \
    --exclude .git \
    --exclude target \
    --exclude .vagrant \
    --exclude index.redb \
    . root@192.168.56.4:ord
  ssh root@192.168.56.4 'cd ord && ./deploy/setup'

time-tests:
  cargo +nightly test -- -Z unstable-options --report-time

profile-tests:
  cargo +nightly test -- -Z unstable-options --report-time \
    | sed -n 's/^test \(.*\) ... ok <\(.*\)s>/\2 \1/p' | sort -n \
    | tee test-times.txt

open:
  open http://localhost

doc:
  cargo doc --all --open

update-ord-dev:
  ./bin/update-ord-dev

rebuild-ord-dev-database: && update-ord-dev
  systemctl stop ord-dev
  rm -f /var/lib/ord-dev/**/index.redb
  journalctl --unit ord-dev --rotate
  journalctl --unit ord-dev --vacuum-time 1s

# publish current GitHub master branch
publish:
  #!/usr/bin/env bash
  set -euxo pipefail
  rm -rf tmp/release
  git clone git@github.com:casey/ord.git tmp/release
  cd tmp/release
  VERSION=`sed -En 's/version[[:space:]]*=[[:space:]]*"([^"]+)"/\1/p' Cargo.toml | head -1`
  git tag -a $VERSION -m "Release $VERSION"
  git push origin $VERSION
  cargo publish
  cd ../..
  rm -rf tmp/release

list-outdated-dependencies:
  cargo outdated -R
  cd test-bitcoincore-rpc && cargo outdated -R

update-modern-normalize:
  curl \
    https://raw.githubusercontent.com/sindresorhus/modern-normalize/main/modern-normalize.css \
    > static/modern-normalize.css

download-log unit='ord' host='ordinals.com':
  ssh root@{{host}} 'mkdir -p tmp && journalctl -u {{unit}} > tmp/{{unit}}.log'
  rsync --progress --compress root@{{host}}:tmp/{{unit}}.log tmp/{{unit}}.log

download-index unit='ord' host='ordinals.com':
  rsync --progress --compress root@{{host}}:/var/lib/{{unit}}/index.redb tmp/{{unit}}.index.redb

graph log:
  ./bin/graph $1

flamegraph dir=`git branch --show-current`:
  ./bin/flamegraph $1

benchmark index height-limit:
  ./bin/benchmark $1 $2

benchmark-revision rev:
  ssh root@ordinals.com "mkdir -p benchmark \
    && apt-get update --yes \
    && apt-get upgrade --yes \
    && apt-get install --yes git rsync"
  rsync -avz benchmark/checkout root@ordinals.com:benchmark/checkout
  ssh root@ordinals.com 'cd benchmark && ./checkout {{rev}}'

build-snapshots:
  #!/usr/bin/env bash
  set -euxo pipefail
  rm -rf tmp/snapshots
  mkdir -p tmp/snapshots
  cargo build --release
  cp ./target/release/ord tmp/snapshots
  cd tmp/snapshots
  for start in {0..750000..50000}; do
    height_limit=$((start+50000))
    if [[ -f $start.redb ]]; then
      cp -c $start.redb index.redb
    fi
    a=`date +%s`
    time ./ord --data-dir . --height-limit $height_limit index
    b=`date +%s`
    mv index.redb $height_limit.redb
    printf "$height_limit\t$((b - a))\n" >> time.txt
  done

serve-docs:
  mdbook serve docs --open

build-docs:
  mdbook build docs

update-changelog:
  git log --pretty='format:- %s' >> CHANGELOG.md
