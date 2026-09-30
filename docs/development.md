# Development environment

texrun は TeX Live / latexmk / PDF preview tool を外部コマンドとして呼び出す。
host に TeX Live を直接インストールしなくても開発・integration test ができるよう、
Rust toolchain と軽量な TeX Live を含む Docker 環境を用意している。
CI（#11）でも同じ構成を使い、TeX distribution の差異による fixture（#10）の揺れを抑えることを目的とする。

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
host に Rust toolchain がある場合は host で直接実行してもよい。コンテナ経由の実行は TeX Live を必要とする作業や、CI と同じ Linux 環境で確認したい場合に使う。

## Cache

再 build を速くするため、以下を named volume に保持している。

| volume | mount 先 | 内容 |
| --- | --- | --- |
| `texrun_cargo-registry` | `/usr/local/cargo/registry` | crates.io の index / crate |
| `texrun_cargo-git` | `/usr/local/cargo/git` | git 依存 |
| `texrun_cargo-target` | `/cargo-target` | build 成果物（`CARGO_TARGET_DIR`） |

コンテナ内では `CARGO_TARGET_DIR=/cargo-target` を設定しているため、host の `target/`（macOS 向け build）とコンテナの build 成果物（Linux 向け）は混ざらない。
成果物のパスは `/cargo-target/debug/...` になる点に注意する。

cache を消したい場合:

```bash
docker compose down --volumes
```

## TeX Live integration test

TeX Live を必要とする integration test の実行方法（`#[ignore]` + `--ignored`、環境変数、cargo feature のいずれで有効化するか）は #10 で決める。
決まり次第このセクションに追記する。いずれの方式でも、この dev コンテナ内で実行すれば `latexmk` / `mutool` / `pdftoppm` が揃った状態でテストできる。

## 注意事項

- コンテナ内の process は root で動く。Docker Desktop / OrbStack（macOS）では bind mount 上に作られたファイルは host ユーザーの所有になるが、Linux の Docker Engine では root 所有になる。build 成果物は volume に出力されるため通常は問題にならない
- この image は開発・テスト用であり、本番の sandbox worker としての container 実行（#9 の post-MVP 範囲）は対象外
