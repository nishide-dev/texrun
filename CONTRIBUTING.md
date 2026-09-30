# Contributing to texrun

texrun への貢献方法と、コミット・PR・Issue の書き方をまとめたドキュメントです。

基本方針は次のとおりです。

- コミット履歴・PR タイトル・Issue タイトルは **英語** にし、機械的に検索・集計しやすくする
- PR 本文・Issue 本文での議論や変更意図は **日本語** で自然に記述する

## 開発フロー

### Branch 運用

- `main` への直接 push は禁止です。すべての変更は feature branch から Pull Request を経由して取り込みます。
- branch は最新の `main` から作成してください。
- branch 名は `<type>/<short-description>` 形式を推奨します（例: `feat/compile-command`, `fix/latexmk-timeout`, `docs/contribution-conventions`）。`<type>` は後述のコミット type と揃えます。

```bash
git switch main
git pull --ff-only
git switch -c feat/compile-command
```

### Merge 方針

- **Squash merge を基本** とします。1 PR = `main` 上の 1 コミットになります。
- リポジトリは以下のように設定されています。
  - squash コミットのタイトル: **PR タイトル**（`squash_merge_commit_title=PR_TITLE`）
  - squash コミットの本文: **PR 本文**（`squash_merge_commit_message=PR_BODY`）
  - merge 後の head branch の自動削除: 有効（`delete_branch_on_merge=true`）
- この設定により、PR 内のコミット数にかかわらず PR タイトルが `main` の最終コミットメッセージになります。そのため PR タイトルは必ず Conventional Commits 形式で書いてください（後述）。
- feature branch 内の個々のコミットは多少粒度が荒くても構いませんが、可能な範囲で Conventional Commits 形式に揃えてください。
- merge 後の feature branch は自動で削除されます。

## Commit message

[Conventional Commits](https://www.conventionalcommits.org/ja/v1.0.0/) 形式を採用します。

```text
<type>(<scope>): <description>
```

scope が不要な場合は、空の `()` を付けずに次の形式にします。

```text
<type>: <description>
```

### 例

scope あり:

```text
feat(cli): add compile command
feat(texlive): add latexmk engine
fix(diagnostics): parse undefined control sequence errors
test(texlive): add timeout fixture
chore(ci): add clippy checks
refactor(core): simplify compile result model
```

scope なし:

```text
docs: document local development setup
chore(deps): update dependencies
build: pin rust toolchain version
```

### type 一覧

| type       | 用途                                               |
| ---------- | -------------------------------------------------- |
| `feat`     | 新機能                                             |
| `fix`      | バグ修正                                           |
| `docs`     | ドキュメントのみの変更                             |
| `test`     | テストの追加・修正                                 |
| `refactor` | 外部仕様を変えないリファクタリング                 |
| `perf`     | 性能改善                                           |
| `chore`    | CI、依存更新、開発環境など                         |
| `build`    | ビルド設定・パッケージング関連（Cargo 設定など）   |

`chore` と `build` の使い分け:

- 依存 crate のバージョン更新は `chore(deps)` とします（例: `chore(deps): bump clap to 4.5`）。
- ビルド設定・パッケージングの変更（`Cargo.toml` の profile や feature、toolchain 指定、配布形態など）は `build` とします。

### scope 候補

scope は変更対象の crate・モジュール・領域を表します。迷った場合は以下から選んでください。

| scope         | 対象                                                   |
| ------------- | ------------------------------------------------------ |
| `cli`         | `texrun` コマンドライン（引数、出力、exit code）       |
| `core`        | compile のドメインモデル・engine interface             |
| `texlive`     | TeX Live / latexmk engine                              |
| `workspace`   | 隔離された compile workspace、path 制御                |
| `diagnostics` | LaTeX log の解析・構造化 diagnostics                   |
| `preview`     | PDF のページ preview・メタデータ取得                   |
| `ci`          | GitHub Actions などの CI 設定                          |
| `docker`      | Docker ベースの開発環境                                |
| `deps`        | 依存 crate の更新（`chore(deps)` として使用）          |

- 複数領域にまたがる場合は、主な変更対象の scope を 1 つ選ぶか、scope を省略してください。
- crate・モジュール構成の変更に伴い scope 候補を追加・変更する場合は、このドキュメントも更新してください。

### description の書き方

- 英語で書く
- 命令形寄りにする（`add`, `fix`, `remove` など。`added` / `adds` は避ける）
- 先頭は小文字、末尾にピリオドを付けない
- 簡潔に（目安として type/scope を含めて 72 文字以内）

必要に応じて本文（body）を空行のあとに記述できます。body は日本語でも構いません。

### Breaking change

外部仕様（CLI の引数・`--json` 出力・exit code など）に互換性のない変更を含む場合は、仕様どおり以下の **どちらか** で表します。

- type/scope の直後に `!` を付ける
- footer（本文末尾の trailer）に `BREAKING CHANGE: <説明>` を記載する

どちらか一方で breaking change として扱われます。変更内容や移行方法を説明したい場合は、`!` と footer を併用して構いません。

```text
feat(cli)!: remove --quiet flag
```

```text
feat(cli): rename --json flag to --format json

BREAKING CHANGE: `--json` は削除され、`--format json` に置き換えられた。
```

```text
feat(cli)!: rename --json flag to --format json

BREAKING CHANGE: `--json` は削除され、`--format json` に置き換えられた。
```

squash merge では PR 本文がコミット本文になるため、`BREAKING CHANGE:` footer を使う場合は PR 本文の末尾に記載してください。

## Pull Request

### PR title

PR タイトルもコミットメッセージと同じ **Conventional Commits 形式の英語** にします。リポジトリ設定により、squash merge 時は PR タイトルがそのまま `main` 上の最終コミットのタイトルになります。

```text
feat(cli): add compile command
fix(texlive): terminate timed-out latexmk process
```

### PR body

PR 本文は **自然な日本語** で記述します。[PR template](.github/pull_request_template.md) に沿って、最低限以下を含めてください。

- **概要**: 何を・なぜ変更するのか
- **変更内容**: 主な変更点
- **確認方法**: 実行したコマンド・手動確認の手順
- **関連 Issue**: `Closes #123` / `Refs #456` など
- **補足**: 必要に応じて、既知の制約・レビュー観点・後続作業など

### レビュー前のチェック

PR を作成・更新する前に、ローカルで品質ゲートを通してください（次節）。

## ローカル品質ゲート

CI と同じ基準をローカルで確認するため、PR 前に以下を実行してください。

```bash
cargo check --workspace --all-targets --all-features
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

フォーマット違反は `cargo fmt --all` で自動修正できます。

CI は `cargo check` / `cargo clippy` / test を `--locked` 付きで実行します。そのため `Cargo.lock` が `Cargo.toml` と一致していないと、CI だけが失敗します。依存を追加・変更したときは、更新された `Cargo.lock` も commit してください。CI と同じ条件で確認したい場合は、上記の `cargo check` / `cargo clippy` / `cargo test` に `--locked` を付けて実行してください（例: `cargo check --workspace --all-targets --all-features --locked`）。

依存 crate を追加・更新した場合は、依存ポリシー（ライセンス・advisory・取得元）も確認してください。

```bash
cargo deny check
```

CI では test を [cargo-nextest](https://nexte.st/) で実行します（`cargo nextest run --workspace --all-features --locked` と、doctest 用の `cargo test --workspace --all-features --locked --doc`）。ローカルでは `cargo test --workspace` で同じ test を実行できます。`cargo-deny` と `cargo-nextest` は `cargo install --locked cargo-deny cargo-nextest` で導入できます。

- 上記は #2 / #11 で定義し、`.github/workflows/ci.yml` で実行している品質ゲートと同一です。CI 側のチェックが変わった場合は、このドキュメントも合わせて更新してください。

- CI を source of truth とし、開発者固有の git hook の導入は必須にしません。

### TeX Live integration test

TeX Live（latexmk）や preview tool（`mutool` / `pdftoppm`）を使う test は、tool が見つからなければ `SKIPPED` と表示して skip します。そのため、上記の `cargo test --workspace` は TeX Live が無い環境でも通ります。
TeX Live を使う compile の経路（engine・fixture・security）や diagnostics parser を変更した場合は、TeX Live を含む Docker 開発環境で skip を失敗に変えて全件を実行してください。CI の `integration` job と同じ条件です。

```bash
docker compose run --rm \
  -e TEXRUN_REQUIRE_TEXLIVE=1 -e TEXRUN_REQUIRE_PREVIEW_TOOLS=1 \
  dev cargo test --workspace --all-features --locked
```

環境変数の意味・fixture の一覧・image の build 方法は [docs/development.md](docs/development.md#tex-live-integration-test) を参照してください。

## Issue

- Issue タイトルは **英語** を基本とします（例: `Add the texrun compile CLI command`）。
- Issue 本文は **日本語** で記述します。
- 実装 Issue には、可能な範囲で以下のセクションを含めてください。
  - **背景**: なぜ必要か
  - **スコープ**: 何をやるか・何をやらないか
  - **完了条件**: 何ができたら close できるか
- バグ報告には、再現手順・期待する挙動・実際の挙動・環境（OS、TeX Live のバージョンなど）を含めてください。

## 自動検証について

main 向けの PR では、GitHub Actions で次の check が自動実行されます。

| check | 内容 |
| ----- | ---- |
| `fmt` | `cargo fmt --all -- --check` |
| `check` | `cargo check --workspace --all-targets --all-features --locked` |
| `clippy` | `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` |
| `test (linux)` / `test (macos)` | `cargo nextest run` と doctest（`cargo test --doc`）。TeX Live / preview tool を使う test は skip される |
| `integration` | Docker 開発環境の image（`docker/dev/Dockerfile`）を build し、その中で `TEXRUN_REQUIRE_TEXLIVE=1` / `TEXRUN_REQUIRE_PREVIEW_TOOLS=1` を付けて `cargo test --workspace --all-features --locked` を実行する（TeX Live integration test、上記） |
| `deny` | `cargo deny --locked check`（advisories / licenses / bans / sources、設定は `deny.toml`） |
| `ci-success` | 上記すべての成功を確認する集約 check |
| `pr-title` | PR タイトルが Conventional Commits 形式か（type は上記の type 一覧に限定、scope は任意、description は小文字始まり・末尾ピリオドなし） |

- merge 前に `ci-success` と `pr-title` が成功していることを確認してください（branch protection の required check として設定する想定です）。
- squash merge では PR タイトルが最終コミットのタイトルになるため、自動検証の対象は PR タイトルです。feature branch 内の個々のコミットメッセージは自動検証しないので、レビュー時に目視で確認します。
- 新しい RustSec advisory を検出するため、`deny` は週次の schedule でも実行されます（schedule のときは `deny` 以外の job はスキップされます）。
- 依存 crate・GitHub Actions・開発環境の base image（digest）の更新は、Dependabot が週次で `chore(deps): ...` の PR を作成します。
- `integration` の image は GitHub Actions の cache（buildx の `type=gha`）から再利用するので、`Dockerfile` を変えない限り build はほぼ cache で済みます。

## License

texrun は [MIT License](LICENSE) のもとで公開されています。コントリビュートされたコードも同ライセンスのもとで提供されるものとします。
