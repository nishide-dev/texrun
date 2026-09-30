# Security model

texrun の信頼境界、MVP で保証する範囲と保証しない範囲、実行制限の既定値をまとめる（#9）。
ここに書いた決定事項は、各実装 Issue（#4 workspace、#5 TeX Live engine、#6 CLI、#8 preview、#10 fixture）が従う仕様である。
実装がこの文書と食い違う場合は、どちらかを修正して揃える。

> **現状:** この文書は方針と検証結果である。
> #5 の担当分（§3.1 の timeout、§3.2 の出力上限、§3.4〜3.7）は `crates/texrun-texlive` で実装した。§3.3 は `crates/texrun-workspace`（#21）で実装した。
> それ以外の実装の進み具合は各 Issue を参照する。
> 公開リポジトリのため、攻撃の再現手順や具体的な入力は書かず、「どの保証をどの層で担保するか」だけを書く。
> 境界を破る方法を見つけた場合は [SECURITY.md](../SECURITY.md) の手順で非公開で報告してほしい。

## 1. 脅威モデル

### 前提

- texrun に渡される TeX ソースと付随ファイル（画像、`.bib`、`.sty` など）は **信頼できない入力** として扱う。
  AI エージェントが生成した文書や、外部から取得したテンプレートを想定する。
- 次のものは信頼する。
  - texrun の利用者（CLI を起動する人・エージェント）
  - texrun のバイナリ
  - host にインストールされた TeX Live
- MVP の TeX engine は texrun と同じ host 上で、同じ OS ユーザー権限の子プロセスとして動く。これを以下「in-process 実行」と呼ぶ。texrun のプロセス内で動くわけではないが、OS レベルの隔離境界は無い。

### 守りたいもの

| 資産 | 例 | 想定する攻撃経路 |
| --- | --- | --- |
| host の任意コマンド実行 | shell、任意バイナリ | TeX の shell escape、latexmk の rc（Perl）、latexmk が補助ツールを起動するときの shell |
| host の秘密情報 | `~/.ssh`、環境変数中の token | TeX のファイル読み込み primitive、root 外 symlink、環境変数の継承 |
| host のファイルの改ざん | `~/.bashrc`、入力プロジェクト自体 | TeX のファイル書き込み primitive |
| host の資源 | CPU、ディスク、メモリ | 無限ループ、巨大ログ、大量ページ |

### 信頼境界

```text
 信頼する                          | 信頼しない
-----------------------------------+------------------------------------------
 texrun CLI / core                 |  入力プロジェクト（コピー元）
 host の TeX Live・latexmk          |    -> workspace（コピー先、texrun が管理）
 texrun が生成する latexmk rc       |        -> TeX engine が読む・実行する内容
 preview tool（mutool / pdftoppm）  |        -> 生成された log / aux / PDF
```

TeX engine（latexmk / pdflatex / bibtex / makeindex）のバイナリは信頼する。
ただし、**信頼できない入力を解釈している最中の engine プロセスは、攻撃者の影響下にある** とみなす。
engine が生成した log・PDF も信頼できない入力として扱う。diagnostics parser（#7）と preview（#8）は、壊れた入力や巨大な入力に耐える必要がある。

## 2. MVP で保証する境界 / 保証しない範囲

### 保証する（この文書の決定事項を実装した時点で）

1. **TeX / latexmk の機能を経由して任意のコマンドを実行させない。** 次の層を重ねて担保する。
   - **TeX の shell escape を無効にする。** `\write18` と pipe による open は実行されない。latexmk の `-no-shell-escape` に加え、texrun 管理 rc の pdflatex コマンドにも明示する（§3.5）。
   - **入力やユーザー環境の rc を実行しない。** 入力に含まれる rc と、user・system の rc は読まない（`-norc`）。代わりに、texrun が workspace の外に生成した rc だけを `-r` で読ませる（§3.5）。
   - **latexmk に shell を使わせない。** latexmk の既定の設定では、pdflatex・bibtex・makeindex などの補助ツールや kpsewhich を shell 経由で起動する。起動の引数には、文書の内容や log に由来する名前が入りうる。texrun 管理 rc では、次のように上書きする。
     - 補助ツールは shell を経由しない（argv を配列のまま渡す）起動に置き換える
     - kpsewhich の呼び出しは無効にする
     - texrun が使わないツールは未実装扱い（`NONE`）にする
   - **多層防御の一段として、entrypoint と output dir の名前を検査する**（§3.5）。この検査だけでは、上の保証は成り立たない。
   - 検証は #10 の security fixture で行う。
2. **workspace の外にあるファイルを、次の経路で読み書きさせない。**
   - `\input` / `\include` / `\openin` / `\openout` による読み書き
   - 画像の読み込み（`\includegraphics` / `\pdfximage`）
   - bibtex の database / style の指定
   - pdfTeX のファイル情報系 primitive（`\pdffilesize` など）

   これらの経路では、次の指定がすべて kpathsea の paranoid mode（`openin_any=p` / `openout_any=p`、§3.7）で拒否されることを確認した。
   - 絶対パス
   - `..` を含むパス
   - dot で始まる component を含むパス

   workspace は入力を texrun 管理のディレクトリにコピーしたもので、root 外を指す symlink は持ち込まない（#4）。
3. **host 由来の環境変数に依存しない。漏らさない**（env allowlist、§3.4）。
4. **資源消費に上限を設ける。** 対象は wall-clock timeout、1 ファイルあたりの書き込みサイズ、output の合計サイズ、入力サイズ（§3.1〜3.3）。
5. **timeout / cancel 時に子孫プロセスを残さない**（process group 単位の kill、§3.6）。

### 保証しない（MVP の in-process 実行の限界）

完全な対策は、将来の sandbox backend（§4、#26）で扱う。

- **font 関連の読み込みは保証しない。** font・font map・encoding などの読み込みには、paranoid mode の検査が及ばない経路がある。
- **PDF object にファイルを直接埋め込む pdfTeX の primitive は保証しない。** この経路は paranoid mode の検査対象外で、workspace 外のファイルを読めることを確認した。
- **TeX Live の texmf ツリー等、host の一部は名前で指定すれば読める。** paranoid mode の判定は「TeX に渡された名前」に対して行う。そのため、各ファイル種別の検索パスで見つかるファイルは読める。検索パスはファイル種別ごとに次のとおり。
  - TeX 入力（`\input` / `\openin`）: `TEXINPUTS`（texmf ツリーの `tex/` 以下と cwd）
  - 画像: `TEXINPUTS`
  - bibtex: `BIBINPUTS` / `BSTINPUTS`（`bibtex/` 以下）
  - font 関連: font 用の各パス（`fonts/` 以下と、OS の font ディレクトリ）

  たとえば `\openin` では、`tex/` 以下の `.sty` / `.cls` は読めた。一方、`web2c/` にある `texmf.cnf`、`ls-R`、font map、`.bst` は見つからなかった。これらのファイルは通常公開情報だが、host 固有の設定やローカルにインストールしたパッケージが含まれうる。
- **OS レベルの隔離は無い。** engine は texrun を起動したユーザーの権限で動く。画像や PDF を解析するコード（pdfTeX・libpng・libjpeg・poppler / MuPDF など）にメモリ安全性の脆弱性があれば、細工した入力で任意コード実行されうる。その場合、そのユーザーが読み書きできるものはすべて危険にさらされる。
- **network は遮断していない**（§3.8、#24）。
- **CPU・メモリは OS レベルで制限していない**（#25）。
  - pdfTeX のメモリは `texmf.cnf` の固定容量（`main_memory` 等）で頭打ちになる。LuaTeX など他の engine はこの限りではない。
  - CPU 時間は timeout でのみ制限する。
- **workspace 内での書き込みは止めない。** TeX は output dir 配下に、任意の名前・任意の数のファイルを作れる。サイズは §3.2 の上限で抑えるが、ファイル数・inode は制限しない。
- **上限に達するまでの資源消費は起こりうる。** ログを出し続ける文書では、ログと stdout がそれぞれ約 32 MB/s の速さで増えた（§6）。
- **process group から抜けるプロセスは追えない。**
  - 新しい session を作った子孫は `killpg` の対象外になる。
  - shell escape を無効にした TeX と、texrun 管理 rc の latexmk は、そのようなプロセスを起動しない。ただし OS として保証するものではない（cgroup 等、#25）。
- **生成物から host の情報が漏れる。** 詳細は §3.9。
- **TeX / latexmk / kpathsea 自体のバグ** によって境界が破れる場合。
- **表示上の偽装。** `WorkspacePath` は bidi 制御文字（U+202A〜U+202E、U+2066〜U+2069）とゼロ幅文字（U+200B〜U+200F、U+FEFF）を許容している。
  - 人間向けの出力でこれらを含む path を表示するときは、escape する（#6）。
  - JSON 出力はそのまま出す。JSON 文字列としては正しく、扱いは消費側の責任とする。
  - output dir と entrypoint の名前では、これらの文字を §3.5 の検査で拒否する。

## 3. 決定事項

各項目の末尾に実装担当 Issue を記す。値は MVP の定数である。timeout 以外を CLI で上書きできるようにするかは #6 で決める。

### 3.1 timeout（#5、CLI 露出は #6）

| 項目 | 既定値 |
| --- | --- |
| compile 全体の wall-clock timeout | **60 秒**（latexmk の全 pass と bibtex / makeindex を含む。`CompileOptions.timeout` が `None` のとき CLI がこの値を入れる） |
| preview 生成の timeout | **30 秒**（全ページの合計。compile の timeout とは別枠、#8） |

- 60 秒は、通常の論文や学会テンプレート（数 pass + bibtex）の compile には十分である。暴走したときの待ち時間としても許容できるので、この値にした。空白 20,000 ページの文書でも、compile は約 9 秒で終わった（§6）。
- timeout の上限値は設けない。利用者が明示的に長くするのは自由とする。`0` は `CompileRequest::validate()` が拒否する（実装済み）。
- timeout を超えたら `CompileOutcome::TimedOut`、cancel されたら `CompileOutcome::Cancelled` を返す。

### 3.2 出力・artifact の上限（#5、preview は #8）

| 項目 | 既定値 | 強制方法 |
| --- | --- | --- |
| 子プロセスが書く 1 ファイルの最大サイズ | **256 MiB** | Linux: `RLIMIT_FSIZE`（下記）。latexmk と全子孫に継承させる。全 OS: output dir の合計サイズと同じ poll で、最大のファイルが上限に達したら process group を kill する |
| output dir の合計サイズ | **1 GiB** | timeout の poll ループ内で定期的に（目安 500 ms ごと）集計し、超えたら process group を kill する |
| stdout / stderr の保持量 | **各 4 MiB**（先頭を保持し、超過分は読み捨てる） | reader thread が pipe を最後まで読み続け、保持する量だけを制限する。pipe を読まずに止めると engine が block する |
| PDF artifact | 256 MiB（`RLIMIT_FSIZE` と同じ値で自動的に頭打ちになる） | — |
| preview のページ数 | 既定は **先頭 20 ページ**。範囲を指定した場合（`--pages`）も **最大 200 ページ** | #8 |
| preview 画像の合計サイズ | **128 MiB**。超えた時点で以降のページを生成せず、warning を出す | #8 |
| preview 画像 1 枚の長辺 | **4096 px**。超えるページは DPI を下げて描画し、info を出す | #8。PDF が宣言するページサイズ（信頼できない値）に関係なく、1 ページあたりのメモリと出力を抑える |

- `RLIMIT_FSIZE` を超えて書き込もうとすると、engine は `SIGXFSZ` で終了する。20 MiB に制限してログを出し続けさせたところ、ログはちょうど 20 MiB で止まり、latexmk は失敗終了した。
- `RLIMIT_FSIZE` の設定方法:
  - 子プロセスの `exec` 前に `setrlimit` する `pre_exec` は `unsafe` で、この workspace は `unsafe_code = "forbid"` なので使わない。
  - 代わりに Linux では、spawn 直後に親から `prlimit(2)` で latexmk に設定する。latexmk が設定前にファイルを書いたり子プロセスを起動したりしないよう、texrun 管理 rc（§3.5）の先頭で stdin から開始の合図を待たせる。親は `prlimit` の後に合図を送る。合図が来ずに stdin が閉じた場合、rc は何もせずに終了する（終了コード 125）。
  - macOS には `prlimit` が無い。1 ファイルの上限は poll（目安 500 ms ごと）でのみ強制するので、検出までの間は上限を超えて書かれうる（ログを出し続ける文書で約 16 MB）。
- `RLIMIT_FSIZE` で書き込みが止まった場合も、終了後の集計で上限に達したファイルを検出し、同じ diagnostic を付ける。
- 設定する値は、texrun 自身の hard limit（子プロセスが継承する値）と上限値の小さい方とする。上限を強める方向にだけ働くので、権限は要らない。
- **core dump は無効にする**（`RLIMIT_CORE=0`、soft・hard とも。Linux で `RLIMIT_FSIZE` と同時に設定する）。`SIGXFSZ` の既定の動作は core dump で、core ファイルは TeX の cwd（workspace 内）に作られ、output dir の集計の対象外になるためである。macOS では `RLIMIT_FSIZE` を設定せず、停止は `SIGKILL` で行うので、core は作られない。
- 上限によって停止した場合は `CompileOutcome::Failed` とし、texrun 由来の diagnostic（「output limit exceeded」等）を付ける。`CompileOutcome` は `#[non_exhaustive]` なので、専用の outcome を追加するかは #5 で判断してよい。

### 3.3 入力の上限と除外（#4、#21 の実装値と揃える）

| 項目 | 既定値 |
| --- | --- |
| workspace にコピーする regular file の合計サイズ | **256 MiB** |
| entry 数（file + directory + symlink） | **10,000** |
| 深さ（root からの path component 数） | **32** |
| 走査する entry 数（除外したものを含む） | **40,000** |

- 値は `WorkspaceConfig` で上書きできる。
- 上限を超えたら、compile を始めずにエラーにする（`WorkspaceError::LimitExceeded`）。黙って一部だけコピーすることはしない。
- コピー時に除外するもの（除外した理由は report に記録する）:
  - VCS のディレクトリ: `.git`、`.hg`、`.svn`
  - `.texrun/`（output root と衝突させないため）
  - root 直下の `target`
  - special file（FIFO / socket 等）
  - `*.fmt` / `*.base` / `*.mem`: workspace 内の format を読み込ませないため。§3.5 の `-no-parse-first-line` と合わせた二重の対策
  - `biber.conf` / `.biber.conf`: 将来 biber を使う場合に、入力から設定を持ち込ませないため
  - `latexmkrc` / `.latexmkrc`: `-norc` で実行はされないが、持ち込まない。report に記録し、CLI が warning として表示する（#6）
- root 外を指す symlink は拒否する（#4 の方針どおり）。

### 3.4 子プロセスの環境変数（#5、preview tool は #8）

`Command::env_clear()` した上で、次の変数 **だけ** を設定する。

| 変数 | 値 | 理由 |
| --- | --- | --- |
| `PATH` | host の `PATH` から、空の entry と相対パスの entry（`.` など）を除いたもの | latexmk は `pdflatex` / `bibtex` を PATH から探す。PATH が空だと起動できなかった。TeX Live のインストール先は host ごとに異なるので、固定値にはしない。cwd は workspace 内なので、相対の entry が残っていると入力に含まれるファイルが TeX のプログラムの代わりに実行されうる |
| `HOME` | `<workspace>/.texrun/home`（texrun が作る空ディレクトリ） | user の `~/texmf`（TEXMFHOME）や `~/.latexmkrc` を使わないため。**未設定にしてはいけない**。`HOME` が無いと、TEXMFHOME と TEXMFVAR が cwd（= workspace）相対のパスになる |
| `openin_any` / `openout_any` | `p` | §3.7 |
| `max_print_line` | `10000` | ログの 79 文字折り返しを抑える（#7 の parse 精度のため）。`error_line` / `half_error_line` も変えるかは #7 で決める |
| `MKTEXTFM` / `MKTEXPK` / `MKTEXMF` / `MKTEXTEX` / `MKTEXFMT` | `0` | ファイルが見つからないときに、kpathsea が生成 script を起動しないようにする。この起動は `-no-shell-escape` とは独立に行われ、生成物を TEXMFVAR に書き込む |
| `LC_ALL` | `C` | メッセージと bibtex の挙動を locale に依存させない。`LC_ALL=C` でも、UTF-8 のファイル名は compile できた |

- 渡さない変数（例）:
  - 検索パス・設定の変数: `TEXINPUTS`・`BIBINPUTS`・`BSTINPUTS`・`TEXMFCNF`・`TEXMF*`。継承すると、入力ファイルの解決先が host 側に変わる
  - shell escape の設定を上書きできる変数: `shell_escape`・`shell_escape_commands`
  - latexmk の system rc を指定する `LATEXMKRCSYS`
  - token などの秘密情報を含みうる、その他の任意の変数
- 再現可能なビルド用の変数（`SOURCE_DATE_EPOCH` / `FORCE_SOURCE_DATE`）は §3.9 で扱う。
- latexmk の実行ファイルを明示的に指定した場合（`LatexmkConfig::latexmk`、#6 の option 候補）も、起動前に絶対パスに解決する（相対パスは texrun の cwd を基準に解決し、symlink も解決する）。相対パスのまま起動すると、子プロセスの cwd（workspace 内）を基準に解決されうる。これは `PATH` の相対 entry を除くのと同じ理由である。
- trade-off: `HOME` を差し替えるので、user が `~/texmf` に入れたパッケージは使えない。必要になったら、明示的な option（例: 追加の読み取り専用 texmf ツリー）として設計する。
- preview tool（`mutool` / `pdfinfo` / `pdftoppm`、#8）には `PATH`・`LC_ALL=C`・`HOME`（preview ごとに作る空の一時ディレクトリ）だけを渡す。kpathsea の変数は不要なので渡さない。tool は検出時に解決した絶対パスで起動し、`PATH` の相対 entry（`.` など）は検出に使わない。

### 3.5 latexmk の起動（#5）

texrun 自身は shell を介さず、argv 配列で latexmk を起動する。

```text
cwd: <entrypoint のあるディレクトリ（workspace 内）>
latexmk -norc -r <texrun 管理 rc（workspace 外）>
        -pdf -interaction=nonstopmode -halt-on-error -file-line-error
        -no-shell-escape
        -outdir=<output dir の絶対パス>
        <entrypoint のファイル名（先頭が - なら ./ を付ける）>
```

**texrun 管理 rc**

rc は compile ごとに、workspace の **外**、texrun が所有する一時ディレクトリに生成し、compile が終わったら（失敗・timeout を含めて）ディレクトリごと削除する。
TeX からは書き込めない（§3.7）。
`-norc` で自動の rc 読み込みを止め、この rc だけを `-r` で読ませる。
rc の中身は固定で、ファイル名などの request 由来の値は埋め込まない。
rc の骨子（設定項目名レベル）は次のとおり。正確な内容は `crates/texrun-texlive/src/rc.rs` にある。

| 設定項目 | 内容 |
| --- | --- |
| 開始の合図の待機（Linux のみ） | stdin から texrun の合図を 1 行読むまで先に進まない。合図が無ければ終了コード 125 で終了する（§3.2） |
| 起動用の Perl sub `texrun_run` | 受け取った引数を、`system { $prog } @args` の形（list 形式、shell を経由しない）で実行する。戻り値は下記 |
| `$pdflatex` | `internal texrun_run pdflatex -no-parse-first-line -no-shell-escape %O %S` |
| `$bibtex` | `internal texrun_run bibtex %O %S` |
| `$makeindex` | `internal texrun_run makeindex %O -o %D %S` |
| `$biber` | `internal texrun_run biber %O %S`（将来用。MVP の image には biber が無い） |
| `$kpsewhich` | `NONE`（呼び出しを無効化する） |
| 上記以外のコマンド変数: `$latex` / `$xelatex` / `$lualatex` / `$dvilualatex` / `$hilatex` / `$dvipdf` / `$dvips` / `$dvips_landscape` / `$ps2pdf` / `$xdvipdfmx` / 各 previewer（`$hnt_previewer` を含む）/ `$dvi_update_command` / `$ps_update_command` / `$pdf_update_command` / `$lpr` / `$lpr_dvi` / `$lpr_pdf` / `$pscmd` / `$make` / `$start_NT` | `NONE`（MVP では使わない） |
| `$pdf_mode` / `$dvi_mode` / `$postscript_mode` | `1` / `0` / `0` |
| `$success_cmd` / `$warning_cmd` / `$failure_cmd` / `$compiling_cmd` のフック、`$dvi_filter` / `$ps_filter`、`$pre_tex_code` | 空にする |
| `$print_type` / `@cus_dep_list` | `none` / 空 |

- `texrun_run` の戻り値: latexmk は、コマンドの戻り値を Perl の `system()` と同じ wait status として扱う（rule の実行では 256 で割って終了コードにする）。そこで sub は次の値を返す。0 を返すのは子プロセスが 0 で終了したときだけである。
  - 終了コード `c` で終了: `c * 256`
  - signal `s` で終了: `(128 + s) * 256`
  - 起動できなかった（`exec` の失敗、引数なし）: `127 * 256`
- `internal` の引数は、latexmk がダブルクォートを考慮して空白で分割する。名前検査（下記）で `"` を拒否するのは、引数を正しく受け渡すためにも必要である。

- 検証結果（§6）: この rc を使うと、プロセスツリーは `latexmk → pdflatex` になり、間に `sh` が入らなかった。次の機能がすべて正常に動いた。
  - 目次
  - 相互参照
  - bibtex
  - サブディレクトリへの `\include`
  - makeindex
- `$kpsewhich = 'NONE'` の副作用が 2 つある。
  - bibtex を使う文書では、latexmk の stderr に `Kpsewhich command needed but not set up` が出る。diagnostics は main の `.log` からだけ作るので、この行は diagnostics にならない。
  - latexmk は、texmf 内のファイルの依存関係を追跡しなくなる。texrun は毎回まっさらな workspace で compile するので、影響は無い。
- latexmk 4.86 のソースで、shell を使う箇所は次の 3 つだった。
  - コマンドの実行
  - kpsewhich の呼び出し
  - `-pvc` 用のプロセス一覧取得

  前の 2 つは、この rc で塞いだ。texrun は `-pv` / `-pvc` を使わない。latexmk を更新するときは、この確認をやり直す。
- **必須オプション**（`-shell-escape` / `-shell-restricted` を有効にする option は提供しない）:
  - `-norc` と `-r <rc>`
  - `-no-shell-escape`
  - `-interaction=nonstopmode`
  - pdflatex の `-no-parse-first-line`（rc で指定する）。TeX Live の既定では pdflatex の `parse_first_line` が `t` で、ソースの先頭行で format を指定できる。このオプションで、先頭行の format 指定が無視されることを確認した。
  - `-e`（Perl を実行する option）は使わない。

**サブディレクトリの entrypoint（例 `src/main.tex`）**

cwd を workspace root にしたままだと、`src/` からの相対 `\input` / `\include` と `.bib` が解決しない。次の 3 方式を比べた。

| 方式 | 結果 |
| --- | --- |
| texrun が cwd を `src/` にし、`-outdir` を絶対パスで渡す | 成功（相互参照・bibtex・makeindex・`\include` を含む） |
| cwd は root のまま、latexmk の `-cd` を使う | 成功。ただし相対 `-outdir` は `src/` 基準で解釈され、output が `src/.texrun/out` にできた。絶対パスなら期待どおりだった |
| cwd は root のまま、`-cd` を使わない | 相対 `\include` と `.bib` が見つからない |

- **決定:** texrun が `Command::current_dir` で cwd を entrypoint のディレクトリにし、`-outdir` は `WorkspaceRoot::output_dir()` の絶対パスで渡す。`-cd` は使わない（latexmk 側の path 解釈に依存しないため）。
- 補足:
  - 相対 `-outdir` で `..` を含む値を渡しても、latexmk は内部で絶対パスに変換していた。
  - rc によって shell を経由しないので、絶対パスに host の temp dir 名が入っても問題にならない。
  - cwd が workspace root のとき（entrypoint が root 直下）、latexmk は絶対パスで渡した `-outdir` を cwd からの相対パスに直して pdflatex に渡す。そのため log には `.texrun/out/main.aux` のような相対名が出る。diagnostics の file は、TeX の cwd（entrypoint のディレクトリ）を基準に解釈してから workspace root 相対に直す（#7 の parser に cwd を root として渡し、結果に entrypoint のディレクトリを前置する）。
- 制約: paranoid mode では `..` を含むパスを読めない。そのため、entrypoint のディレクトリより上にあるファイル（例 `\input{../common/macros}`）は読めない。そうした構成では、project root に置いた entrypoint から読み込むよう案内する（#6 の CLI ヘルプ・エラーメッセージ）。

**output dir のサブディレクトリの事前作成**

pdflatex の `-output-directory` は、サブディレクトリを作らない。
latexmk はログの特定の行を見てサブディレクトリを作る機能を持つが、`-file-line-error` を付けるとその行の形式が変わって検出されず、`\include{chapters/intro}` の compile が失敗した（paranoid mode とは無関係）。

- 対策（#5）: latexmk を起動する前に、entrypoint のディレクトリ配下のディレクトリ構造を output dir 配下に作っておく。
  - output dir 自身とその祖先のディレクトリ、workspace root の `.texrun`（output root と engine の `HOME`）は写さない。
  - symlink はたどらない。
- この対策で、`-file-line-error` を維持したまま成功した。`-file-line-error` を外す案は #7 の parse を難しくするので採らない。

**entrypoint と output dir の名前の検査（多層防御）**

上の rc によって、名前が shell に渡ることは無くなる。
それでも、rc の設定漏れや latexmk の将来の変更に備えて、engine は entrypoint と output dir（workspace 相対）の各文字を検査する。
外れたら `EngineError::InvalidRequest` にする。

- ASCII: 英数字、空白、`.` `_` `-` `+` `,` `=` `@` `/` だけを許可する。shell や TeX で特別な意味を持つ記号（引用符、`` ` ``、`$`、`\`、`;`、`|`、`&`、`<`、`>`、括弧類、`*`、`?`、`~`、`#`、`%`、`^`、`!` など）は拒否する。
- 非 ASCII: Unicode の general category が **Cf（format。bidi 制御文字・ゼロ幅文字を含む）** の文字だけを拒否し、それ以外は許可する。制御文字（Cc）は `WorkspacePath` が既に拒否している。日本語の全角記号・全角空白・結合文字（NFD）は許可する。
- **Unicode 正規化はしない。** 判定は与えられた文字列のまま行い、path もそのまま使う。
  - Linux のファイル名はバイト列で、正規化して別の名前にすると、実在するファイルを指せなくなる。
  - macOS（APFS）は正規化の違いを区別しない。
  - 結合文字は許可しているので、NFC / NFD のどちらの形でも同じ判定になる。
- 先頭の `-` は `WorkspacePath::to_cli_arg()` で `./-x.tex` になり、正しく扱われることを確認した。latexmk は `--` をサポートしない（`--` を渡すと失敗する）。
- latexmk 自身も `$` を含む名前は拒否するが、空白を含む名前は通す。latexmk 側の検査には依存しない。
- 同じ検査を、latexmk に渡す host 側の絶対パス（workspace root・cwd・output dir）にも行う。host の temp dir 名に `"` などが入っていると、`internal` の引数分割で壊れるためである。エラーメッセージには、該当する path と文字を含める（利用者は `TMPDIR` などで temp dir を変えられる）。

### 3.6 プロセスの停止（#5、preview tool は #8）

- spawn 時に新しい process group を作る（`CommandExt::process_group(0)`）。
- timeout / cancel（`CancelToken`）/ 出力上限の超過時は、**process group に `SIGKILL` を送る**（`killpg`）。
  - latexmk だけに `SIGTERM` を送ると、その子プロセスが親 PID 1 のまま走り続けることを確認した。
  - TeX に graceful shutdown は不要で、途中までの log はファイルに残る。そのため `SIGTERM` による猶予は設けない。
- latexmk が正常終了した後も `killpg(SIGKILL)` を 1 回送り、残った子孫を掃除する（`ESRCH` は無視する）。
  - **leader を reap する前に `killpg` する。** leader の終了は `waitid(P_PID, WEXITED | WNOHANG | WNOWAIT)` で検知し、reap しない。leader の zombie が PID と PGID を確保しているので、`killpg` が無関係な process group に届くことは無い。
  - `killpg` の後で leader を reap する。timeout / cancel / 上限超過の場合も同じ順序で行う。
- `ctx.cancel.is_cancelled()`、timeout、出力サイズは、同じ poll ループで確認する（#20 からの申し送りどおり）。
- stdout / stderr の reader thread は、process group を kill した後に最大 2 秒だけ待つ。group から抜けたプロセスが pipe を開いたままでも、compile は終わる（それまでに読めた分を返す）。

### 3.7 kpathsea 設定（#5）

- `openin_any=p` / `openout_any=p` を環境変数で渡す。TeX Live / Debian の既定は `openin_any=a`、`openout_any=p`。
- `TEXMFOUTPUT` は設定しない。設定すると、そのディレクトリ配下の絶対パスへの書き込みが許可される。
- **output dir `.texrun/out`（dot ディレクトリ配下）は paranoid mode と両立する。**
  - 判定の対象は TeX に渡された名前（`main.aux`、`chapters/intro.aux`）で、`-output-directory` の prefix は判定の後に付く。
  - `-outdir` を相対で渡しても絶対で渡しても、次のものが正常に書け、相互参照と引用も解決された。
    - `.aux`・`.log`・`.toc`
    - `\include` した子ファイルの `.aux`
    - bibtex の出力（`.bbl` / `.blg`）
    - makeindex の出力（`.ind` / `.ilg`）
  - **既定の output dir を変える必要はない。**
- 副作用: 文書から、dot で始まる path（`\input{.hidden/x}`）や `..` を含む path は読めなくなる。互換性の制約として受け入れる。

### 3.8 network（#24）

- compile と preview は、network を必要としない前提で設計する。package の自動インストール（`tlmgr`、MiKTeX の on-the-fly install 相当）は行わない。
- MVP では network を遮断しない（§2）。どう強制するかは #24 で扱い、sandbox backend（#26）では network 無しを既定にする。

### 3.9 生成物に含まれる host の情報（#4 / #5 / #6）

生成物には、次の host の情報が含まれる。

| 生成物 | 含まれる情報 |
| --- | --- |
| `.fls` / `.fdb_latexmk` | workspace の絶対パス（`PWD` 行。host の temp dir 名。macOS では user ごとのパスを含む） |
| `.log` | workspace と texmf ツリーの絶対パス |
| PDF | 作成日時（`CreationDate` / `ModDate`）、それに依存する `/ID`、pdfTeX と TeX Live の版を表す banner（`PTEX.Fullbanner`、`Producer`） |

これらの扱いは次のとおりとする。

- 収集する artifact は engine が報告したもの（PDF、log、preview）だけにする。`.fls` / `.fdb_latexmk` / `.aux` は収集しない（#4 / #5）。
- 既定では `SOURCE_DATE_EPOCH` / `FORCE_SOURCE_DATE` を設定しないので、PDF の日時は compile した時刻になる。再現可能なビルドを求められた場合に限り、CLI の option（#6）で `SOURCE_DATE_EPOCH=<値>` と `FORCE_SOURCE_DATE=1` を env allowlist に加える。これで、PDF の日時が固定されることを確認した。
- banner と log 内のパスは、MVP では抑制しない。生成物を第三者と共有する場合は、利用者の判断に委ねる。

## 4. 将来の sandbox backend（#26）

MVP の in-process 実行の次に、TeX engine を texrun 本体と別の isolation boundary で動かす backend を追加する。

| 候補 | 得られるもの | 主な課題 |
| --- | --- | --- |
| Docker / Podman（rootless） | filesystem・network・PID の namespace、cgroup による CPU / memory / pids 制限、texmf の read-only 提供 | daemon / runtime の有無、起動のオーバーヘッド、macOS では VM を経由する |
| gVisor（`runsc`） | container の隔離に加え、syscall を user-space kernel で仲介して kernel の攻撃面を縮小する | Linux 限定、I/O 性能 |
| microVM（Firecracker 等） | VM 境界による強い隔離 | image と起動の管理が複雑、KVM が必須 |

移行方針:

1. sandbox backend は、`TypesetEngine` の別実装（例: container 内で latexmk を動かす engine）として追加する。
   - §3 の設定（texrun 管理 rc・env allowlist・kpathsea 設定・各種上限）は、sandbox 内でもそのまま適用する。
   - その上に、OS 側の制限を **追加で** 重ねる。
     - network 無し（#24）
     - read-only の root filesystem
     - texmf の read-only mount
     - cgroup の memory / pids 上限（#25）
     - non-root user での実行
   - §2 で「保証しない」とした font 関連の読み込みと、PDF object 埋め込み系の primitive は、sandbox の filesystem 隔離で担保する。
2. workspace の作成・入力のコピー・artifact の収集（#4）は backend 共通のまま、workspace root を sandbox 内に mount する。
3. 将来 xelatex / lualatex / dvipdfmx を追加するときは、それぞれが起動する外部プログラムも §3.5 と同じ基準で扱う。
   - xelatex は出力 driver（`xdvipdfmx`）を子プロセスとして起動する。
   - dvipdfmx は画像変換に外部プログラム（Ghostscript 等）を使う設定を持つ。
   - LuaTeX は Lua からのプロセス起動・ファイルアクセスを持つ。

   それぞれについて、次のことを確認する。
   - shell を経由せず起動されるか
   - 変換コマンドの設定を固定できるか
   - restricted / safer 系の option（例: LuaTeX の `--safer` 相当）が使えるか

   必要なら rc と env allowlist を拡張する。

core API（`CompileContext`）が backend の差し替えに耐える理由:

- `CompileRequest` と `Artifact.path` は、`WorkspacePath`（workspace root 相対のパス）だけでファイルを参照し、host の絶対パスを含まない。sandbox 内の mount 先（例: `/work`）が host と違っても、request と result は変わらない。
- host 側の実行時情報は `CompileContext` にまとめてあり、`TypesetEngine::compile(&self, ctx, req)` の引数を増やさない設計になっている。`CompileContext` は `#[non_exhaustive]` で、`with_*` builder から作る。そのため、sandbox 固有の情報（path mapping、resource limit、runtime の指定など）を field として追加しても、既存の engine と呼び出し側は壊れない。
- timeout は `CompileOptions`（backend 非依存）で、cancel は `CancelToken`（poll 型）で表している。どちらも、subprocess でも container でも同じ意味で実装できる。
- `CompileOutcome` と `EngineError` は `#[non_exhaustive]` である。backend 固有の失敗（例: runtime が無い場合の `EngineError::Unavailable`）も、既存の分類で表せる。

## 5. 脆弱性の報告

報告窓口は [SECURITY.md](../SECURITY.md) に記載する（GitHub の private vulnerability reporting を使う）。

## 6. 実験結果の要約

**環境:** 開発用 Docker image（`docs/development.md`）で確認した。

- OS: Debian 13.7 trixie（arm64）
- TeX Live パッケージ:
  - `texlive-binaries` 2024.20240313（pdfTeX 1.40.26。banner の表記は `TeX Live 2025/dev/Debian`、kpathsea 6.4.0）
  - `texlive-latex-base` / `-recommended` 2024.20250309
- latexmk 4.86

コンテナ内では root で実行した。kpathsea の判定は名前ベースなので、root でも結果は変わらない。
TeX Live の版や OS が違う場合は、確認をやり直すこと。
公開リポジトリのため、境界を破る入力の具体例は載せず、「どの層で止まったか」だけを書く。
実験用のファイルは repo に置かず、コンテナ内の `/tmp` で作業した。

| 確認項目 | 結果 |
| --- | --- |
| rc の読み込み | `-norc` が無いと、workspace の `latexmkrc` / `.latexmkrc` と `$HOME/.latexmkrc` の Perl が実行された。`-norc -r <texrun 管理 rc>` では、workspace・user・system（Debian では `/etc/LatexMk`）のどの rc も実行されず、指定した rc だけが読まれた |
| TeX の shell escape（`-no-shell-escape`） | `runsystem(...)...disabled.` となり、実行されなかった。pipe による open も実行されなかった |
| TeX Live 既定の restricted shell escape | `shell_escape=p` で、`shell_escape_commands` に列挙されたコマンド（bibtex, bibtex8, extractbb, gregorio, kpsewhich, l3sys-query, latexminted, makeindex, memoize-extract.pl, memoize-extract.py, repstopdf, r-mpost, texosquery-jre8）は実行される。texrun は `-no-shell-escape` でこれも無効にする |
| 環境変数による shell escape 設定の上書き | 継承すると設定を変えられた。`-no-shell-escape` と env allowlist の両方で防ぐ |
| latexmk 既定設定での補助ツールの起動 | pdflatex・bibtex・makeindex が `sh -c` 経由で起動され、名前に含まれる shell メタ文字が解釈された |
| texrun 管理 rc での補助ツールの起動 | 同じ入力でも shell を経由せず、メタ文字は解釈されなかった。プロセスツリーは `latexmk → pdflatex` |
| texrun 管理 rc での通常文書 | 目次・相互参照・bibtex・`\include`（サブディレクトリ）・makeindex がすべて成功した。サブディレクトリの entrypoint も、cwd を entrypoint のディレクトリにする方式で成功した |
| 先頭行の format 指定 | `-no-parse-first-line` が無いと先頭行の指定に従って別の format が読まれ、あると無視された |
| paranoid mode: TeX の読み書き primitive、画像、bibtex、pdfTeX のファイル情報系 primitive | 絶対パス、`..`、dot で始まる component を含む指定は、すべて拒否された（既定の `openin_any=a` では読めた） |
| paranoid mode: 名前での検索 | `\openin` では、texmf の `tex/` 以下のファイルは読めた。`texmf.cnf`・`ls-R`・font map・`.bst` は見つからなかった。texmf の sty / cls / font（`.pfb`）/ `pdftex.map` の通常の読み込みは壊れなかった |
| paranoid mode の検査が及ばない経路 | font 関連と、PDF object にファイルを埋め込む primitive（§2「保証しない」） |
| output dir `.texrun/out` + paranoid | `-outdir` が相対でも絶対でも、aux / log / toc / bibtex / makeindex / `\include` した子ファイルの `.aux` が成功した（サブディレクトリを事前に作った場合。§3.5） |
| `\include{chapters/intro}` + `-file-line-error` | サブディレクトリが無いと、paranoid mode の有無に関係なく失敗した。`-file-line-error` を外すと、latexmk がサブディレクトリを自動で作った |
| 空の環境（`env -i`） | pdflatex を起動できずに失敗した |
| `PATH` だけの環境 | 成功した（bibtex と相互参照を含む）。`HOME` が無いと、TEXMFHOME / TEXMFVAR が cwd 相対になった |
| 検索パスの変数（`TEXINPUTS`）の継承 | host 側に置いたファイルが、標準のクラスファイルの代わりに読まれた。`env -i` では読まれなかった |
| `max_print_line=10000` | 既定では 79 文字で折り返されるメッセージが、1 行で出た |
| 存在しないフォント | kpathsea が生成 script を起動した。`MKTEX*=0` にすると起動しなかった |
| 無限ループ（`\def\x{\x}\x`） | CPU を使い続けて終わらない。latexmk だけに `SIGTERM` を送ると子プロセスが残り、process group に `SIGKILL` を送ると全て止まった |
| memory を使い続ける展開 | 数秒で `TeX capacity exceeded, sorry [main memory size=5000000]` になって止まった |
| ログを出し続けるループ | 5 秒でログが約 160 MB、latexmk の stdout も約 160 MB になった |
| `RLIMIT_FSIZE` 20 MiB で同上 | ログが 20 MiB で止まり、latexmk は失敗終了した |
| 空白 20,000 ページ | 約 9 秒で成功した。PDF は 3.5 MB、ログは 170 KB |
| 生成物の host 情報 | `.fls` に workspace の絶対パスが入った。PDF には作成日時と pdfTeX / TeX Live の banner が入った。`SOURCE_DATE_EPOCH=0` と `FORCE_SOURCE_DATE=1` で、日時が固定された |
