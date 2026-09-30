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
- squash 後のコミットメッセージには **PR タイトル** がそのまま使われるため、PR タイトルは Conventional Commits 形式で書いてください（後述）。
- そのため feature branch 内の個々のコミットは多少粒度が荒くても構いませんが、可能な範囲で Conventional Commits 形式に揃えてください。
- merge 後の feature branch は削除します。

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
chore: update dependencies
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
| `build`    | ビルドシステムやパッケージ関連（Cargo 設定など）   |

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

- 複数領域にまたがる場合は、主な変更対象の scope を 1 つ選ぶか、scope を省略してください。
- crate・モジュール構成の変更に伴い scope 候補を追加・変更する場合は、このドキュメントも更新してください。

### description の書き方

- 英語で書く
- 命令形寄りにする（`add`, `fix`, `remove` など。`added` / `adds` は避ける）
- 先頭は小文字、末尾にピリオドを付けない
- 簡潔に（目安として type/scope を含めて 72 文字以内）

必要に応じて本文（body）を空行のあとに記述できます。body は日本語でも構いません。

### Breaking change

外部仕様（CLI の引数・`--json` 出力・exit code など）に互換性のない変更を含む場合は、type/scope の直後に `!` を付け、body 末尾に `BREAKING CHANGE:` を記載します。

```text
feat(cli)!: rename --json flag to --format json

BREAKING CHANGE: `--json` は削除され、`--format json` に置き換えられた。
```

## Pull Request

### PR title

PR タイトルもコミットメッセージと同じ **Conventional Commits 形式の英語** にします。squash merge 時にそのまま最終コミットメッセージとして利用されることを意図しています。

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
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

必要に応じて以下も利用できます。

```bash
# フォーマットの自動修正
cargo fmt --all

# 型チェックのみを素早く行う
cargo check --workspace --all-targets --all-features
```

- CI を source of truth とし、開発者固有の git hook の導入は必須にしません。
- TeX Live が必要な integration test の実行方法は、開発環境の整備（Docker ベースの TeX Live 環境など）に合わせて README 等に記載します。

## Issue

- Issue タイトルは **英語** を基本とします（例: `Add the texrun compile CLI command`）。
- Issue 本文は **日本語** で記述します。
- 実装 Issue には、可能な範囲で以下のセクションを含めてください。
  - **背景**: なぜ必要か
  - **スコープ**: 何をやるか・何をやらないか
  - **完了条件**: 何ができたら close できるか
- バグ報告には、再現手順・期待する挙動・実際の挙動・環境（OS、TeX Live のバージョンなど）を含めてください。

## 自動検証について

コミットメッセージおよび PR タイトルが Conventional Commits 形式であることの自動検証は、CI 整備（#11）の一環として導入する予定です。それまではレビュー時に目視で確認します。

## License

texrun は [MIT License](LICENSE) のもとで公開されています。コントリビュートされたコードも同ライセンスのもとで提供されるものとします。
