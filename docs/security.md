# Security model

texrun の信頼境界、MVP で保証する範囲と保証しない範囲、実行制限の既定値をまとめる（#9）。
ここに書いた決定事項は、各実装 Issue（#4 workspace、#5 TeX Live engine、#6 CLI、#8 preview）が従う仕様である。
実装がこの文書と食い違う場合は、どちらかを修正して揃える。

> **現状:** この文書は方針と検証結果であり、ここに書いた制限の多くはまだ実装されていない。
> 実装の進み具合は各 Issue を参照する。

## 1. 脅威モデル

### 前提

- texrun に渡される TeX ソースと付随ファイル（画像、`.bib`、`.sty` など）は **信頼できない入力** として扱う。
  AI エージェントが生成した文書や、外部から取得したテンプレートを想定する。
- texrun の利用者（CLI を起動する人・エージェント）と texrun のバイナリ、host にインストールされた TeX Live は信頼する。
- MVP の TeX engine は texrun と同じ host 上で、同じ OS ユーザー権限の子プロセスとして動く（以下「in-process 実行」と呼ぶ。texrun のプロセス内ではないが、OS レベルの隔離境界は無い）。

### 守りたいもの

| 資産 | 例 | 想定する攻撃 |
| --- | --- | --- |
| host の任意コマンド実行 | shell、任意バイナリ | `\write18`、`latexmkrc`（Perl）、ファイル名経由の shell injection |
| host の秘密情報 | `~/.ssh`、`/etc/passwd`、環境変数中の token | `\input{/etc/passwd}`、`\openin`、root 外 symlink、環境変数の継承 |
| host のファイルの改ざん | `~/.bashrc`、入力プロジェクト自体 | `\openout` による workspace 外への書き込み |
| host の資源 | CPU、ディスク、メモリ | 無限ループ、巨大ログ、大量ページ |

### 信頼境界

```text
 信頼する                          | 信頼しない
-----------------------------------+------------------------------------------
 texrun CLI / core                 |  入力プロジェクト（コピー元）
 host の TeX Live・latexmk          |    -> workspace（コピー先、texrun が管理）
 preview tool（mutool / pdftoppm）  |        -> TeX engine が読む・実行する内容
                                   |        -> 生成された log / aux / PDF
```

TeX engine（latexmk / pdflatex / bibtex）は信頼するバイナリだが、**信頼できない入力を解釈している最中の engine プロセスは攻撃者の影響下にある** とみなす。
engine が生成した log・PDF も信頼できない入力として扱う（diagnostics parser #7、preview #8 は壊れた・巨大な入力に耐える必要がある）。

## 2. MVP で保証する境界 / 保証しない範囲

### 保証する（この文書の決定事項を実装した時点で）

1. **任意コマンド実行をしない（TeX / latexmk の機能として）**
   - `\write18` / `\immediate\write18` は実行されない（`-no-shell-escape`。restricted shell escape も無効）。
   - `\input{"|cmd"}` / `\openout` の pipe は実行されない（shell escape 無効時は pipe も無効）。
   - 入力に含まれる `latexmkrc` / `.latexmkrc`、user・system の rc ファイルは実行されない（`-norc`）。
   - latexmk は内部で `sh -c` を使うため、entrypoint 名・output dir 名に shell メタ文字を含めない（§3.5）。
2. **TeX から workspace 外のファイルを名前で指定して読み書きさせない**
   - 絶対パス（`/etc/passwd`、`/proc/self/environ`）、`..` を含むパス、dot で始まる component を含むパスの `\input` / `\openin` / `\openout` は kpathsea の paranoid mode（`openin_any=p` / `openout_any=p`）で拒否される。
   - workspace は入力を texrun 管理のディレクトリにコピーしたもので、root 外を指す symlink は持ち込まない（#4）。
3. **host 由来の環境変数に依存しない・漏らさない**（env allowlist、§3.4）。
4. **資源消費に上限がある**: wall-clock timeout、1 ファイルあたりの書き込みサイズ上限、output の合計サイズ上限、入力サイズ上限（§3.1〜3.3）。
5. **timeout / cancel 時に子孫プロセスを残さない**（process group 単位の kill、§3.6）。

### 保証しない（MVP の in-process 実行の限界）

- **TeX Live の texmf ツリー等、host の一部は読める。**
  paranoid mode の判定は「TeX に渡された名前」に対して行われ、kpathsea の検索で見つかるファイル（`\input{article}` → `/usr/share/texlive/texmf-dist/...`）は読める。
  つまり TEXMFDIST・TEXMFLOCAL・TEXMFSYSVAR・TEXMFSYSCONFIG（Debian では `/usr/share/texlive`、`/usr/share/texmf`、`/var/lib/texmf`、`/etc/texmf`）配下のファイルは文書から読み出して PDF に埋め込める。
  これらは通常公開情報だが、host 固有の設定やローカルにインストールしたパッケージが含まれうる。
- **OS レベルの隔離は無い。** engine は texrun を起動したユーザーの権限で動く。pdfTeX・libpng・libjpeg・poppler / MuPDF など、画像や PDF を解析するコードにメモリ安全性の脆弱性があれば、細工した画像・PDF で任意コード実行されうる。その場合、そのユーザーが読み書きできるものはすべて危険にさらされる。
- **network は遮断していない。** TeX Live + latexmk の compile は network を必要としないが、network namespace 等で禁止はしていない。
- **CPU・メモリは OS レベルで制限していない。** pdfTeX のメモリは `texmf.cnf` の固定容量（`main_memory` 等）で頭打ちになる（`\def\x{x\x}\x` は数秒で `TeX capacity exceeded` で止まった）が、LuaTeX など他 engine はこの限りではない。CPU 時間は timeout でのみ制限する。
- **workspace 内での書き込みは止めない。** TeX は workspace 内（output dir 配下）に任意の名前・数のファイルを作れる。サイズは §3.2 の上限で抑えるが、ファイル数・inode は制限しない。
- **timeout までの資源消費は起こりうる。** ログを吐き続ける文書は約 32 MB/s でログと stdout を生成した（§6）。上限に達するまでの消費は許容する。
- **process group から抜けるプロセスは追えない。** `setsid` 等で新しい session を作った孫プロセスは killpg の対象外になる。shell escape 無効の TeX / latexmk はそのようなプロセスを起動しないが、OS として保証するものではない（cgroup 等は post-MVP）。
- **TeX / latexmk / kpathsea 自体のバグ** による境界の破れ。
- **表示上の偽装。** `WorkspacePath` は bidi 制御文字（U+202A〜U+202E、U+2066〜U+2069）やゼロ幅文字（U+200B〜U+200F、U+FEFF）を許容している。人間向け出力でこれらを含む path を表示するときは escape する（#6）。JSON 出力はそのまま出す（JSON 文字列として正しく、消費側の責任で扱う）。

## 3. 決定事項

各項目の末尾に実装担当 Issue を記す。値は MVP の定数であり、CLI で上書きできるようにするか（timeout 以外）は #6 で決める。

### 3.1 timeout（#5、CLI 露出は #6）

| 項目 | 既定値 |
| --- | --- |
| compile 全体の wall-clock timeout | **60 秒**（latexmk の全 pass・bibtex を含む。`CompileOptions.timeout` が `None` のとき CLI がこの値を入れる） |
| preview 生成の timeout | **30 秒**（全ページ合計。compile の timeout とは別枠、#8） |

- 60 秒は通常の論文・学会テンプレート（数 pass + bibtex）に十分で、暴走時の待ち時間として許容できる値として選んだ。空白 20,000 ページの文書でも約 9 秒で compile が終わった（§6）。
- timeout の上限値は設けない（利用者が明示的に長くするのは自由）。`0` は `CompileRequest::validate()` が拒否する（実装済み）。
- 超過時は `CompileOutcome::TimedOut`、cancel 時は `CompileOutcome::Cancelled`。

### 3.2 出力・artifact の上限（#5、preview は #8）

| 項目 | 既定値 | 強制方法 |
| --- | --- | --- |
| 子プロセスが書く 1 ファイルの最大サイズ | **256 MiB** | `RLIMIT_FSIZE`（spawn 前に `pre_exec` で `setrlimit`。latexmk と全子孫に継承される） |
| output dir の合計サイズ | **1 GiB** | timeout の poll ループ内で定期的（目安 500 ms ごと）に集計し、超えたら process group を kill |
| stdout / stderr の保持量 | **各 4 MiB**（先頭を保持し、超過分は読み捨てる） | reader thread が pipe を最後まで読み続け、保持量だけを制限する（pipe を止めると engine が block する） |
| PDF artifact | 256 MiB（`RLIMIT_FSIZE` と同じ値で自動的に頭打ち） | — |
| preview のページ数 | 既定 **先頭 20 ページ**、指定範囲（`--pages`）でも **最大 200 ページ** | #8 |
| preview 画像の合計サイズ | **128 MiB** を超えた時点で以降のページを生成せず warning | #8 |

- `RLIMIT_FSIZE` を超えた書き込みでは engine が `SIGXFSZ` で終了する（20 MiB に制限してログを吐き続けさせると、ログはちょうど 20 MiB で止まり latexmk は失敗終了した）。
- 上限による停止は `CompileOutcome::Failed` とし、texrun 由来の diagnostic（「output limit exceeded」等）を付ける。専用の outcome を追加するかは #5 で判断してよい（`CompileOutcome` は `#[non_exhaustive]`）。

### 3.3 入力の上限（#4）

| 項目 | 既定値 |
| --- | --- |
| workspace にコピーする合計サイズ | **512 MiB** |
| ファイル数 | **10,000** |

- 上限を超えたら compile を始めずに `EngineError::InvalidRequest` 相当のエラーにする（黙って一部だけコピーしない）。
- コピー時の除外: `.git/`、`.texrun/`（output root と衝突させない）、`latexmkrc` / `.latexmkrc`（`-norc` で実行はされないが持ち込まない。除外した旨を warning にする）。
- root 外を指す symlink は拒否（#4 の方針どおり）。

### 3.4 子プロセスの環境変数（#5、preview tool は #8）

`Command::env_clear()` した上で、次の変数 **だけ** を設定する。

| 変数 | 値 | 理由 |
| --- | --- | --- |
| `PATH` | host の `PATH` をそのまま | latexmk が `pdflatex` / `bibtex` を PATH から探す。空だと `kpathsea: Can't get directory of program name: ./pdflatex` で失敗した。TeX Live のインストール先は host ごとに異なるので固定値にしない |
| `HOME` | `<workspace>/.texrun/home`（texrun が作る空ディレクトリ） | user の `~/texmf`（TEXMFHOME）や `~/.latexmkrc` を使わない。**未設定にしてはいけない**: `HOME` が無いと TEXMFHOME が `./texmf`、TEXMFVAR が `./.texlive2025/texmf-var`（cwd = workspace 相対）になる |
| `openin_any` | `p` | §3.7 |
| `openout_any` | `p` | §3.7 |
| `max_print_line` | `10000` | ログの 79 文字折り返しを抑える（#7 の parse 精度）。`error_line` / `half_error_line` も変える場合は #7 で決める |
| `MKTEXTFM` / `MKTEXPK` / `MKTEXMF` | `0` | 存在しないフォント名で kpathsea が `mktextfm` 等の shell script を起動し、TEXMFVAR に書き込むのを止める（`-no-shell-escape` とは独立に動く） |
| `LC_ALL` | `C` | メッセージ・bibtex の挙動を locale に依存させない。`LC_ALL=C` でも UTF-8 のファイル名（`日本語.tex`）は compile できた |

- 渡さないもの（例）: `TEXINPUTS`・`BIBINPUTS`・`BSTINPUTS`・`TEXMFCNF`・`TEXMF*`（host の設定で入力の解決先が変わる。`TEXINPUTS` を継承すると host 側の `article.cls` に差し替えられることを確認した）、`shell_escape`（**環境変数 `shell_escape=t` を継承すると、`-no-shell-escape` 無しでは `\write18` が実行された**）、token・秘密情報を含みうる任意の変数。
- `SOURCE_DATE_EPOCH` / `FORCE_SOURCE_DATE`（再現可能な PDF）は必要になった時点で allowlist に追加を検討する。
- trade-off: `HOME` を差し替えるため、user が `~/texmf` に入れたパッケージは使えない。必要になったら明示的な option（例: 追加の読み取り専用 texmf ツリー）として設計する。

### 3.5 latexmk の起動（#5）

```text
latexmk -pdf -norc -no-shell-escape
        -interaction=nonstopmode -halt-on-error -file-line-error
        -outdir=<output_dir（workspace 相対）>
        <entrypoint（WorkspacePath::to_cli_arg()）>
```

- 必須: `-norc`、`-no-shell-escape`、`-interaction=nonstopmode`。`-shell-escape` / `-shell-restricted` を有効にする option は提供しない。
- texrun 自身は shell を介さず argv 配列で起動し、working directory は workspace root にする。
- `-outdir` は **workspace 相対** で渡す（相対・絶対のどちらでも動作は同じだった。相対にすると host の temp dir のパスが latexmk の内部 shell command に入らない）。
- **latexmk は pdflatex / bibtex を `sh -c` 経由で起動する**（`Running 'pdflatex ... "main.tex"'`。ファイル名は二重引用符で囲まれるだけ）。entrypoint にバッククォートを含む `` g`id>pwn_g`.tex `` を渡すと **`id` が実行された**。`-outdir` にバッククォートを含めても同様に実行された。latexmk 自身は `$` や空白を含む名前を拒否するが、バッククォートは通してしまう。
  - そのため engine は entrypoint と output dir の各文字を allowlist で検査し、外れたら `EngineError::InvalidRequest` にする: ASCII 英数字、`.` `_` `-` `+` `,` `=` `@` `/`、および非 ASCII の英数字（`char::is_alphanumeric`）。空白、`` ` `` `$` `"` `'` `\` `;` `|` `&` `<` `>` `(` `)` `{` `}` `[` `]` `*` `?` `~` `#` `%` `^` `!` などは拒否する（TeX 側でも問題を起こす文字が多い）。
  - 先頭 `-` は `WorkspacePath::to_cli_arg()` で `./-x.tex` になり、正しく扱われることを確認した。latexmk は `--` をサポートしない（`--` を渡すと失敗する）。
- **`-file-line-error` と `\include{chapters/intro}` の組み合わせで subdirectory の `.aux` が書けない問題**: pdflatex の `-output-directory` は subdirectory を作らない。latexmk は `! I can't write on file ...` というログ行を見て subdirectory を作って再実行する機能を持つが、`-file-line-error` を付けるとこの行が `./main.tex:6: I can't write on file ...` になって検出されず、compile が失敗する（paranoid mode とは無関係に起きる）。
  - 対策（#5）: **latexmk 起動前に、workspace 内の入力ディレクトリ構造を output dir 配下に作っておく**（`chapters/` があれば `<output_dir>/chapters/` を作る）。これで `-file-line-error` を維持したまま相対・絶対 `-outdir` の両方で成功した。`-file-line-error` を外す案は #7 の parse を難しくするので採らない。
  - `-e '$allow_subdir_creation=2'` は検出条件が同じなので効かなかった（`-e` は Perl を実行する option でもあり、使わない）。

### 3.6 プロセスの停止（#5、preview tool は #8）

- spawn 時に新しい process group を作る（`CommandExt::process_group(0)`）。
- timeout / cancel（`CancelToken`）/ 出力上限超過時は **process group に `SIGKILL`**（`killpg`）。latexmk に `SIGTERM` を送るだけでは、子の `sh -c` と `pdflatex` が親 PID 1 のまま走り続けることを確認した。TeX に graceful shutdown は不要で、途中までの log はファイルに残るので、`SIGTERM` による猶予は設けない。
- latexmk が正常終了した後も `killpg(SIGKILL)` を 1 回送り、残った子孫を掃除する（`ESRCH` は無視）。group に生存メンバーがいる間は同じ ID の process group は作られないので、leader を reap した後の `killpg` でも無関係なプロセスには届かない。
- `ctx.cancel.is_cancelled()` と timeout・出力サイズは同じ poll ループで確認する（#20 からの申し送りどおり）。

### 3.7 kpathsea 設定（#5）

- `openin_any=p` / `openout_any=p` を環境変数で渡す（TeX Live / Debian の既定は `openin_any=a`、`openout_any=p`）。
- `TEXMFOUTPUT` は設定しない（設定するとそのディレクトリ配下の絶対パスへの書き込みが許可される）。
- **output dir `.texrun/out`（dot ディレクトリ配下）は paranoid mode と両立する。** 判定対象は TeX に渡された名前（`main.aux`、`chapters/intro.aux`）で、`-output-directory` の prefix は判定の後に付くため。相対・絶対の `-outdir` の両方で `.aux`・`.log`・`.toc`・`\include` 子ファイルの `.aux`・bibtex（`.bbl` / `.blg`）が正常に書け、相互参照と引用が解決された。**既定 output dir の変更は不要**。
- 副作用: 文書から dot で始まる path（`\input{.hidden/x}`）や `..` を含む path は読めなくなる（互換性の制約として受け入れる）。

### 3.8 network（post-MVP で強制）

- compile・preview は network を必要としない前提で設計する。package の自動インストール（`tlmgr`、MiKTeX の on-the-fly install 相当）は行わない。
- MVP では network を遮断しない（§2）。sandbox backend では network 無しを既定にする。

## 4. 将来の sandbox backend

MVP の in-process 実行の次に、TeX engine を texrun 本体と別の isolation boundary で動かす backend を追加する。

| 候補 | 得られるもの | 主な課題 |
| --- | --- | --- |
| Docker / Podman（rootless） | filesystem・network・PID namespace、cgroup による CPU / memory / pids 制限、texmf を read-only で提供 | daemon / runtime の有無、起動オーバーヘッド、macOS では VM 経由 |
| gVisor（`runsc`） | container に加え syscall を user-space kernel で仲介し、kernel 攻撃面を縮小 | Linux 限定、I/O 性能 |
| microVM（Firecracker 等） | VM 境界による強い隔離 | image・起動管理の複雑さ、KVM 必須 |

移行方針:

1. sandbox backend は `TypesetEngine` の別実装（例: container 内で latexmk を動かす engine）として追加する。§3 の latexmk 引数・env allowlist・kpathsea 設定・上限は sandbox 内でもそのまま適用し、OS 側の制限（`--network none`、read-only root、texmf の read-only mount、cgroup の memory / pids 上限、non-root user）を **追加で** 重ねる。
2. workspace の作成・入力のコピー・artifact の収集（#4）は backend 共通のまま、workspace root を sandbox 内に mount する。

core API（`CompileContext`）が差し替えに耐える理由:

- `CompileRequest` と `Artifact.path` は `WorkspacePath`（workspace root 相対のパス）だけで file を参照し、host の絶対パスを含まない。sandbox 内での mount 先（例: `/work`）が host と違っても、request と result は変わらない。
- host 側の実行時情報は `CompileContext` にまとめ、`TypesetEngine::compile(&self, ctx, req)` の引数を増やさない設計になっている。`CompileContext` は `#[non_exhaustive]` で `with_*` builder から作るので、sandbox 固有の情報（path mapping、resource limit、runtime の指定など）を field として追加しても既存の engine・呼び出し側は壊れない。
- timeout は `CompileOptions`（backend 非依存）、cancel は `CancelToken`（poll 型）で表しており、subprocess でも container でも同じ意味で実装できる。
- `CompileOutcome` / `EngineError` は `#[non_exhaustive]` で、backend 固有の失敗（runtime が無い = `EngineError::Unavailable` 等）を既存の分類で表せる。

## 5. 脆弱性の報告

報告窓口は [SECURITY.md](../SECURITY.md) に記載する（GitHub の private vulnerability reporting を使う）。

## 6. 実験結果の要約

**環境:** 開発用 Docker image（`docs/development.md`）。Debian 13.7 trixie（arm64）の TeX Live パッケージ: `texlive-binaries` 2024.20240313（pdfTeX 1.40.26、banner 表記は `TeX Live 2025/dev/Debian`、kpathsea 6.4.0）、`texlive-latex-base` / `-recommended` 2024.20250309、latexmk 4.86。コンテナ内 root で実行した（kpathsea の判定は名前ベースで、root でも結果は変わらない）。TeX Live の版・OS が違う場合は再確認すること。

| 確認項目 | 結果 |
| --- | --- |
| workspace の `latexmkrc` / `.latexmkrc`、`$HOME/.latexmkrc` | `-norc` なし: Perl の `system()` が実行された。`-norc` あり: どれも実行されなかった（`-norc` は system rc も読まない。Debian の system rc は `/etc/LatexMk`） |
| `\write18` / `\immediate\write18`（`-no-shell-escape`） | どちらも `runsystem(...)...disabled.` で実行されない |
| 同上（オプションなし = TeX Live 既定） | `shell_escape=p`（restricted）。`echo` は `disabled (restricted)`、許可リスト（`shell_escape_commands` = bibtex, bibtex8, extractbb, gregorio, kpsewhich, l3sys-query, latexminted, makeindex, memoize-extract.pl, memoize-extract.py, repstopdf, r-mpost, texosquery-jre8）の `kpsewhich` は `executed safely` で実行された |
| 環境変数 `shell_escape=t` | `-no-shell-escape` なしでは `\write18` が実行された。`-no-shell-escape` があれば実行されない |
| `\input{/etc/passwd}` | 既定（`openin_any=a`）では読めて PDF に中身が出た。`openin_any=p` では ``File `/etc/passwd.tex' not found`` |
| `\openin` | `openin_any=p` で `/etc/hostname`、`/proc/self/environ`、`../`、`sub/../x`、`.hidden`、`.texrun/out/x.log`、texmf 内の絶対パスは拒否。`article.cls`（名前指定）は読めた。`"\|id"` の pipe は実行されない |
| `\openout`（`openout_any=p`） | `/tmp/...`、`../...`、`.dotfile.txt`、`sub/.hid.txt`、workspace 内の絶対パスは ``I can't write on file`` で拒否。`normal.txt`、`sub/n.txt` は output dir 配下に書かれた |
| output dir `.texrun/out` + paranoid | `-outdir` 相対・絶対とも aux / log / toc / bibtex / `\include` 子の `.aux` が成功（subdirectory を事前作成した場合。§3.5）。fonts（Type1 `.pfb`）、`pdftex.map`（`/var/lib/texmf`）、texmf の `.sty` / `.cls` も問題なく読めた |
| `\include{chapters/intro}` + `-file-line-error` | subdirectory が無いと paranoid の有無に関係なく失敗。`-file-line-error` を外すと latexmk が自動で作る |
| `env -i`（空の環境） | `kpathsea: Can't get directory of program name: ./pdflatex` で失敗 |
| `env -i PATH=/usr/bin:/bin` | 成功（bibtex・相互参照含む）。`HOME` 無しだと TEXMFHOME / TEXMFVAR が cwd 相対になる |
| `TEXINPUTS` の継承 | host 側に置いた `article.cls` が読み込まれた。`env -i` では読まれない |
| `max_print_line=10000` | 既定では 79 文字で折り返された長い `\typeout` が 1 行で出た |
| 存在しないフォント | kpathsea が `mktextfm` を起動した。`MKTEXTFM=0` 等で起動しない |
| entrypoint / `-outdir` のバッククォート | latexmk の `sh -c` で実行された（§3.5）。`$`・空白を含む名前は latexmk 自身が拒否 |
| `\def\x{\x}\x` | CPU を使い続けて終わらない。プロセスツリーは `latexmk`（perl）→ `sh -c pdflatex ...` → `pdflatex` で、全員が latexmk の process group に属する。latexmk だけに `SIGTERM` を送ると `sh` と `pdflatex` が残り、`kill -KILL -<pgid>` で全て消えた |
| `\def\x{x\x}\x` | 数秒で `TeX capacity exceeded, sorry [main memory size=5000000]` で停止 |
| `\typeout` の無限ループ | 5 秒でログ約 160 MB、latexmk の stdout も約 160 MB |
| `ulimit -f`（`RLIMIT_FSIZE`）20 MiB で同上 | ログが 20 MiB で止まり、latexmk は失敗終了 |
| 空白 20,000 ページ | 約 9 秒で成功。PDF 3.5 MB、ログ 170 KB |

再現手順（コンテナ内、作業は `/tmp` 配下で行い repo には置かない）:

```bash
docker-compose run --rm dev    # docs/development.md 参照（環境によっては docker compose）
mkdir -p /tmp/w && cd /tmp/w
printf '%s\n' '\documentclass{article}\begin{document}' '\input{/etc/passwd}' '\end{document}' > rd.tex
openin_any=p openout_any=p latexmk -pdf -norc -no-shell-escape \
  -interaction=nonstopmode -file-line-error -outdir=.texrun/out rd.tex
grep -m1 'not found' .texrun/out/rd.log      # File `/etc/passwd.tex' not found.
```

他の項目も同様に、上の表の入力を 1 ファイルずつ作り、`env -i PATH=... openin_any=p openout_any=p latexmk <§3.5 の引数>` で実行してログ（`<outdir>/*.log`）と生成ファイルの有無を確認する。
プロセスツリーは `setsid latexmk ... loop.tex &` の後に `ps -o pid,ppid,pgid,args --forest -g <sid>` で確認できる。
