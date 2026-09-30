# Development environment

texrun は TeX Live / latexmk / PDF preview tool を外部コマンドとして呼び出す。
host に TeX Live を直接インストールしなくても開発・integration test ができるよう、
Rust toolchain と軽量な TeX Live を含む Docker 環境を用意している。
CI の `integration` job も同じ image を build して使い、TeX distribution の差異による fixture の揺れを抑える（[TeX Live integration test](#tex-live-integration-test)）。そのほかの CI job（fmt / check / clippy / test / deny）は TeX Live を必要としないため、この image は使わず runner 上で直接実行している。

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

Rust / TeX Live のバージョンを変更する場合は、`docker/dev/Dockerfile` の `FROM` 行（`rust:<version>-slim-<Debian release>@sha256:<digest>`）と `rust-toolchain.toml` を揃えて更新する。
base image は multi-arch index の digest で固定している。同じ tag の digest の更新は Dependabot（docker ecosystem）が PR を作る。tag（Rust の version）は `rust-toolchain.toml` と一緒に手で上げる（Dependabot では version の更新を無視している）。
digest（multi-arch index のもの）は `docker buildx imagetools inspect rust:<tag>` の `Digest:` 行で確認できる。buildx が必要なので、buildx の無い環境（OrbStack 同梱の `docker` など）では、`docker pull rust:<tag>` の後に `docker image inspect rust:<tag> --format '{{json .RepoDigests}}'` で確認する（`docker manifest inspect rust:<tag>` は index の中身の platform ごとの digest を表示するもので、index 自体の digest は出ない）。

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

`crates/texrun-preview` の実ツールを使う test（`tests/real_tools.rs`）は、`mutool` / `pdftoppm` が見つからない backend を skip する。
- skip した backend は、test binary ごとに 1 回だけ stderr に `SKIPPED` と表示する。
  - libtest の出力 capture を通さずに書くので、`cargo test` では `--nocapture` を付けなくても表示される。
  - `cargo nextest run` は test ごとに process を分けて出力を capture する。`.config/nextest.toml` で、この test binary と `crates/texrun-texlive` の test は成功時の出力も実行の最後にまとめて表示するようにしてある（test ごとに process が分かれるので、`SKIPPED` 行は binary ごとではなく test ごとに出る）。
- test 自体は pass 扱いになる。CI の `test (linux)` / `test (macos)` の runner には tool が無いので、そこでは skip される。
- 両方がそろっているコンテナや CI の `integration` job では、skip を失敗にして確実に実行させる（TeX Live の test と同じ方式。[TeX Live integration test](#tex-live-integration-test)）:

```bash
docker compose run --rm -e TEXRUN_REQUIRE_PREVIEW_TOOLS=1 dev cargo test -p texrun-preview
```

## Cache

| 対象 | 保存先 | 共有範囲 |
| --- | --- | --- |
| build 成果物（`CARGO_TARGET_DIR`） | `target/docker/`（bind mount 内、コンテナからは `/workspace/target/docker`） | checkout ごと |
| cargo registry（crates.io の index / crate） | named volume `texrun_cargo-registry`（`/usr/local/cargo/registry`） | 全 checkout で共有 |
| cargo git 依存 | named volume `texrun_cargo-git`（`/usr/local/cargo/git`） | 全 checkout で共有 |

- build 成果物は checkout ごとに分離している。cargo は local package の鮮度を mtime で判定し、どの checkout もコンテナ内では同じ `/workspace` に見えるため、target を checkout 間で共有すると別 worktree の古い成果物で test が通ってしまう（偽 green）おそれがある
- `target/docker/` は `.gitignore` の `/target/` で無視される。host で build した `target/debug/` などとは別ディレクトリなので、macOS 向けと Linux 向けの成果物は混ざらない
- registry / git の cache は内容で識別されるため共有しても安全。`compose.yaml` で project 名を `texrun` に固定しているので、どの checkout から実行しても同じ volume を使う
- bind mount 上の target でも build 時間の差は小さい。OrbStack（macOS, Apple Silicon）で clean な `cargo clippy` + `cargo test` を測ると、named volume では約 2.4 秒、bind mount では約 2.6 秒だった。これは現状の小さな workspace で測った参考値で、Docker の実装・host の file system・マシン性能によって変わる

cache の消し方:

```bash
# この checkout のコンテナ用 build 成果物だけを消す
docker compose run --rm dev cargo clean

# registry / git の cache volume も消す
docker compose down --volumes
```

`docker compose down --volumes` は project 名が固定なので、どの checkout で実行しても全 checkout 共有の registry / git cache が消える（次回は再ダウンロードになる）。各 checkout の `target/docker/` は消えない。

## TeX Live integration test

実際の TeX Live / preview tool を使う test は、`#[ignore]` や cargo feature ではなく **実行時の検出 + 環境変数** で選別する（#10）。

| 環境変数 | 対象 | tool が無いとき（未設定） | `=1` のとき |
| --- | --- | --- | --- |
| `TEXRUN_REQUIRE_TEXLIVE` | `crates/texrun-texlive` の TeX Live を使う test（`latexmk` を検出） | skip | 失敗 |
| `TEXRUN_REQUIRE_PREVIEW_TOOLS` | `crates/texrun-preview` の実ツール test、`crates/texrun-texlive` の PDF ページ数と preview の確認 | skip | 失敗 |

- tool があれば、環境変数が無くても test は実行される。host に TeX Live がある場合、`cargo test --workspace` で host の TeX Live を使って実行される（version の違いで失敗した場合は、コンテナ内の結果を正とする）。
- skip したときは、test binary ごとに 1 回だけ stderr に `SKIPPED ... (set TEXRUN_REQUIRE_...=1 to fail instead)` と表示する。libtest の capture を通さずに書くので `cargo test` でそのまま見える。`cargo nextest run`（CI の `test (linux)` / `test (macos)`）では、`.config/nextest.toml` の設定で成功した test の出力も実行の最後に表示されるので、そこに `SKIPPED` 行が出る。nextest は test ごとに process を分けるため、この場合は binary ごとではなく test ごとに 1 行出る。nextest の集計の `skipped` は 0 のままである（test としては pass 扱い）。
- `=1` 以外の値は未設定と同じ扱いになる。
- CI の `integration` job は両方を `1` にしてコンテナ内で全 test を実行するので、tool が無いことで黙って成功することはない。
- TeX Live / preview tool を使う test に `#[ignore]` は使わない。`integration` job は `--ignored` を付けないので、ignore された test は CI で一度も実行されない。これを防ぐため、job は test の出力に ignored が 1 件でもあれば（`test result: ... N ignored` の N が 1 以上）失敗する。

dev コンテナ内で TeX Live の test を含めて全件実行する（CI の `integration` job と同じ条件）:

```bash
docker compose run --rm \
  -e TEXRUN_REQUIRE_TEXLIVE=1 -e TEXRUN_REQUIRE_PREVIEW_TOOLS=1 \
  dev cargo test --workspace --all-features --locked
```

TeX Live engine の test だけを実行する:

```bash
docker compose run --rm -e TEXRUN_REQUIRE_TEXLIVE=1 -e TEXRUN_REQUIRE_PREVIEW_TOOLS=1 \
  dev cargo test -p texrun-texlive
```

### test と fixture の構成

`crates/texrun-texlive/tests/` に置いている。

| ファイル | 内容 |
| --- | --- |
| `common/mod.rs` | 選別（`require_texlive!`）、fixture の compile、構造化結果の assert などの共通 helper |
| `scenarios.rs` | 典型的な文書の fixture（下表）を workspace 経由で compile し、結果を検証する |
| `security.rs` | [docs/security.md](security.md) §2 の保証が、workspace + engine の経路で効いていることを検証する |
| `latexmk.rs` | engine 自体の挙動（probe、cancel、プロセスツリー、出力上限など） |
| `fixtures/<name>/` | fixture の TeX project。そのまま project root として `Workspace::create` に渡す（コピーされるので、test が fixture の隣に書き込むことはない） |

| fixture | 確認すること |
| --- | --- |
| `minimal` | 成功、PDF と log の artifact（size・PDF 1 ページ）、`collect_artifacts` で host にコピーされる |
| `syntax-error` | 環境の対応の誤り: `Failed`、`latex_error` の file / line |
| `undefined-command` | `Failed`、`undefined_control_sequence` の file / line |
| `missing-package` | `Failed`、`missing_file`（file のみ。TeX は停止位置しか出さない） |
| `references` | bibtex（biber は使わない）で解決した引用と、未定義の参照・引用の warning（`Succeeded` のまま） |
| `overfull-box` | `overfull_box` warning の file / line |
| `timeout` | 無限ループを短い timeout（2 秒）で `TimedOut` にし、プロセスを残さない |
| `multi-file` | サブディレクトリの `\input` / `\include`: workspace へのコピー、子ファイルの `.aux`、ページ数、子ファイルに帰属する diagnostic、preview の生成 |
| `unicode-names` | 日本語・空白・全角空白を含むファイル名 |
| `security/*` | shell escape、workspace 外の読み書き、rc ファイル、先頭行の format 指定、補助ツールの起動 |

- 検証は構造化された結果（outcome、diagnostic の kind / file / line、artifact の有無と size、PDF のページ数）に対して行い、ログ全文の snapshot には依存しない。TeX Live の minor な差で壊れにくくするためである。
- PDF のページ数は `texrun-preview`（`mutool` または `pdfinfo`）で読む。
- fixture を追加・変更したときは、コンテナ内で上のコマンドを実行して確認する。

## 注意事項

- コンテナ内の process は root で動く。Docker Desktop / OrbStack（macOS）では bind mount 上に作られたファイルは host ユーザーの所有になるが、Linux の Docker Engine では root 所有になる。integration test（#10）の出力先は repo 内ではなく tempdir にする
- Linux の Docker Engine では、`target/` が無い状態でコンテナを実行すると `target/` 自体が root 所有で作られ、host の `cargo build` が Permission denied で失敗する。これを避けるため、初回は host で先に作っておく

  ```bash
  mkdir -p target
  ```

  すでに root 所有になってしまった場合は、所有者を戻す（`target/docker/` だけを消したい場合は `docker compose run --rm dev cargo clean` でもよい）

  ```bash
  sudo chown -R "$(id -u):$(id -g)" target
  ```

- `docker compose run --rm --user "$(id -u):$(id -g)" dev ...` のように host ユーザーで実行する方法は、現状ではサポートしていない。OrbStack で試したところ、registry の cache がすでにある場合は `cargo test` が通った。しかし cache が空の volume では、`cargo fetch` が root 所有の `/usr/local/cargo/registry` に書き込めず、Permission denied で失敗した。HOME も未設定になる。対応する場合は、image 側で `CARGO_HOME` と volume の権限を調整する必要があるため、将来の選択肢とする
- image tag `texrun-dev:latest` は全 checkout で共有している。`Dockerfile` を変更した checkout で build すると、ほかの checkout が使う image も置き換わる。変更前の image に戻すときは、元の checkout で `docker compose build dev` をやり直す
- base image は digest で固定しているが、apt パッケージは version を固定していない（image を build した時点の Debian の版が入る）。fixture（#10）の揺れを調べるときの参考として、現時点の主な version を記録しておく（Debian 13.7 trixie, arm64）。CI の `integration` job は `Tool versions` step で実際の version を表示する
  - `texlive-binaries` 2024.20240313.70630+ds-6（pdfTeX 1.40.26）
  - `texlive-latex-base` / `texlive-latex-recommended` 2024.20250309-1
  - `latexmk` 4.86、`mupdf-tools` 1.25.1、`poppler-utils` 25.03.0
- この image は開発・テスト用であり、本番の sandbox worker としての container 実行（#9 の post-MVP 範囲）は対象外
