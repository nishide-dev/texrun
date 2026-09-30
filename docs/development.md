# Development environment

texrun は TeX Live / latexmk / PDF preview tool を外部コマンドとして呼び出す。
host に TeX Live を直接インストールしなくても開発・integration test ができるよう、
Rust toolchain と軽量な TeX Live を含む Docker 環境を用意している。
TeX Live が必要な CI の integration job（#11 の後続、#10 の fixture が揃った段階で追加）でも同じ構成を使い、TeX distribution の差異による fixture の揺れを抑えることを目的とする。現在の CI（fmt / check / clippy / test / deny）は TeX Live を必要としないため、この image は使わず runner 上で直接実行している。

## 構成

| ファイル | 内容 |
| --- | --- |
| `docker/dev/Dockerfile` | dev image の定義 |
| `compose.yaml` | service `dev`、bind mount と cache volume の定義 |

image に含まれるもの:

- Rust 1.98.1（`rust:1.98.1-slim-trixie` ベース）+ `rustfmt` / `clippy`
  - `rust-toolchain.toml`（channel = "1.98.1"、components = rustfmt / clippy）と一致させており、コンテナ内で toolchain の再ダウンロードは発生しない
- TeX Live（Debian パッケージ）: `latexmk`, `texlive-latex-base`, `texlive-latex-recommended`
  - `pdflatex` と `bibtex` が利用できる。`biber` は含まない
  - image サイズを抑えるため `--no-install-recommends` とし、ドキュメント類（`/usr/share/doc`、TeX Live の `doc/` など）は入れていない
- PDF preview tool（#8）: MuPDF `mutool`（`mupdf-tools`）、Poppler `pdftoppm` / `pdfinfo`（`poppler-utils`）

Rust / TeX Live のバージョンを変更する場合は `docker/dev/Dockerfile` の `RUST_VERSION` / `DEBIAN_RELEASE` と `rust-toolchain.toml` を揃えて更新する。

## 前提

- Docker（Docker Desktop / OrbStack / Linux の Docker Engine）と Compose v2 以降
- 以下のコマンド例は `docker compose` で記載している。環境によっては `docker-compose` に読み替える

## Image の build

```bash
docker compose build dev
```

`Dockerfile` を変更したときも同じコマンドで再 build する。

## コマンドの実行

repo root で `docker compose run --rm dev <command>` を実行する。repo は `/workspace` に bind mount され、そこが working directory になる。

```bash
# ローカル品質ゲート（CONTRIBUTING.md と同一）
docker compose run --rm dev cargo check --workspace --all-targets --all-features
docker compose run --rm dev cargo fmt --all -- --check
docker compose run --rm dev cargo clippy --workspace --all-targets --all-features -- -D warnings
docker compose run --rm dev cargo test --workspace

# TeX Live / preview tool の確認
docker compose run --rm dev latexmk -v
docker compose run --rm dev mutool -v
docker compose run --rm dev pdftoppm -v

# 対話 shell
docker compose run --rm dev
```

品質ゲートの定義は [CONTRIBUTING.md](../CONTRIBUTING.md#ローカル品質ゲート) が正であり、変更された場合はこちらも合わせて更新する。
CI と同じ条件で確認したい場合は `cargo check` / `cargo clippy` / `cargo test` に `--locked` を付ける。
CI で使っている `cargo-deny` / `cargo-nextest` は image に含めていない。`cargo deny check` や `cargo nextest run` は host で実行する（test 自体はコンテナ内の `cargo test --workspace` で同じものを実行できる）。
host に Rust toolchain がある場合は host で直接実行してもよい。コンテナ経由の実行は TeX Live を必要とする作業や、CI と同じ Linux 環境で確認したい場合に使う。

## Cache

| 対象 | 保存先 | 共有範囲 |
| --- | --- | --- |
| build 成果物（`CARGO_TARGET_DIR`） | `target/docker/`（bind mount 内、コンテナからは `/workspace/target/docker`） | checkout ごと |
| cargo registry（crates.io の index / crate） | named volume `texrun_cargo-registry`（`/usr/local/cargo/registry`） | 全 checkout で共有 |
| cargo git 依存 | named volume `texrun_cargo-git`（`/usr/local/cargo/git`） | 全 checkout で共有 |

- build 成果物は checkout ごとに分離している。cargo は local package の鮮度を mtime で判定し、どの checkout もコンテナ内では同じ `/workspace` に見えるため、target を checkout 間で共有すると別 worktree の古い成果物で test が通ってしまう（偽 green）おそれがある
- `target/docker/` は `.gitignore` の `/target/` で無視される。host で build した `target/debug/` などとは別ディレクトリなので、macOS 向けと Linux 向けの成果物は混ざらない
- registry / git の cache は内容で識別されるため共有しても安全。`compose.yaml` で project 名を `texrun` に固定しているので、どの checkout から実行しても同じ volume を使う
- bind mount 上の target でも build 時間の差は小さい。OrbStack（macOS, Apple Silicon）で clean な `cargo clippy` + `cargo test` を測ると、named volume では約 2.4 秒、bind mount では約 2.6 秒だった（現状の小さな workspace で計測）

cache の消し方:

```bash
# この checkout のコンテナ用 build 成果物だけを消す
docker compose run --rm dev cargo clean

# registry / git の cache volume も消す
docker compose down --volumes
```

`docker compose down --volumes` は project 名が固定なので、どの checkout で実行しても全 checkout 共有の registry / git cache が消える（次回は再ダウンロードになる）。各 checkout の `target/docker/` は消えない。

## TeX Live integration test

TeX Live を必要とする integration test の実行方法（`#[ignore]` + `--ignored`、環境変数、cargo feature のいずれで有効化するか）は #10 で決める。
決まり次第このセクションに追記する。いずれの方式でも、この dev コンテナ内で実行すれば `latexmk` / `mutool` / `pdftoppm` が揃った状態でテストできる。

## 注意事項

- コンテナ内の process は root で動く。Docker Desktop / OrbStack（macOS）では bind mount 上に作られたファイルは host ユーザーの所有になるが、Linux の Docker Engine では root 所有になる。Linux では `target/docker/` も root 所有になるため、消すときは `docker compose run --rm dev cargo clean` を使う。integration test（#10）の出力先は repo 内ではなく tempdir にする
- image tag `texrun-dev:latest` は全 checkout で共有している。`Dockerfile` を変更した checkout で build すると、ほかの checkout が使う image も置き換わる。変更前の image に戻すときは、元の checkout で `docker compose build dev` をやり直す
- base image は tag 指定（digest 固定なし）で、apt パッケージも version を固定していない。fixture（#10）の揺れを調べるときの参考として、現時点の主な version を記録しておく（Debian 13.7 trixie, arm64）
  - `texlive-binaries` 2024.20240313.70630+ds-6（pdfTeX 1.40.26）
  - `texlive-latex-base` / `texlive-latex-recommended` 2024.20250309-1
  - `latexmk` 4.86、`mupdf-tools` 1.25.1、`poppler-utils` 25.03.0
- この image は開発・テスト用であり、本番の sandbox worker としての container 実行（#9 の post-MVP 範囲）は対象外
