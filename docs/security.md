# Security model

texrun の信頼境界、MVP で保証する範囲と保証しない範囲、実行制限の既定値をまとめる（#9）。
ここに書いた決定事項は、各実装 Issue（#4 workspace、#5 TeX Live engine、#6 CLI、#8 preview、#10 fixture）が従う仕様である。
実装がこの文書と食い違う場合は、どちらかを修正して揃える。

> **現状:** この文書は方針と検証結果である。
> #5 の担当分（§3.1 の timeout、§3.2 の出力上限、§3.4〜3.7）は `crates/texrun-texlive` で実装した。§3.3 は `crates/texrun-workspace`（#21）で実装した。
> 外部プロセスの制限付き実行（§3.2 の rlimit、§3.4 の env allowlist、§3.6 の停止、§3.10 の CPU・memory・プロセス数の上限）は、latexmk と preview tool で共通の `crates/texrun-process`（#32、#25）にまとめてある。
> §4 の sandbox backend（container backend、#26）は、container の制限を `crates/texrun-sandbox`、container 内で latexmk を動かす engine を `crates/texrun-texlive`（`ContainerEngine`）に実装した。
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
- MVP の TeX engine は texrun と同じ host 上で、同じ OS ユーザー権限の子プロセスとして動く。これを以下「in-process 実行」（CLI の `--backend host`、既定）と呼ぶ。texrun のプロセス内で動くわけではないが、OS レベルの隔離境界は無い。
- sandbox backend（`--backend container`、§4）では、TeX engine は container の中で動く。container runtime（Docker / Podman）と engine image は信頼する。

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

この節の「保証する」「保証しない」は、両方の backend に共通の in-process 実行についてのものである。sandbox backend（`--backend container`）は、同じ設定をそのまま container の中で適用し、その上に OS の隔離を重ねる。sandbox backend で追加で保証するものは、下の「sandbox backend で追加で保証する」にまとめた。

### 保証する（この文書の決定事項を実装した時点で）

1. **TeX / latexmk の機能を経由して任意のコマンドを実行させない。** 次の層を重ねて担保する。
   - **TeX の shell escape を無効にする。** `\write18` と pipe による open は実行されない。latexmk の `-no-shell-escape` に加え、texrun 管理 rc の pdflatex コマンドにも明示する（§3.5）。
   - **入力やユーザー環境の rc を実行しない。** 入力に含まれる rc と、user・system の rc は読まない（`-norc`）。代わりに、texrun が workspace の外に生成した rc だけを `-r` で読ませる（§3.5）。
   - **latexmk に shell を使わせない。** latexmk の既定の設定では、pdflatex・bibtex・makeindex などの補助ツールや kpsewhich を shell 経由で起動する。起動の引数には、文書の内容や log に由来する名前が入りうる。texrun 管理 rc では、次のように上書きする。
     - 補助ツールは shell を経由しない（argv を配列のまま渡す）起動に置き換える
     - kpsewhich の呼び出しは無効にする
     - texrun が使わないツールは未実装扱い（`NONE`）にする
   - **多層防御の一段として、entrypoint と output dir の名前を検査する**（§3.5）。この検査だけでは、上の保証は成り立たない。
   - 検証は #10 の security fixture で行う（下の「保証の検証」）。
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
4. **資源消費に上限を設ける。** 対象は次のとおり。
   - wall-clock timeout、1 ファイルあたりの書き込みサイズ、output の合計サイズ、入力サイズ（§3.1〜3.3）
   - 各プロセスの CPU 時間（Linux・macOS）と address space（Linux）（§3.10）
   - Linux で委譲された cgroup が使える場合は、engine とその子孫全体の memory・プロセス数・CPU の同時使用（§3.10）

   上限に達した compile は `CompileOutcome::Failed` になり、`resource_limit` の diagnostic が付く（§3.10）。
5. **timeout / cancel 時に子孫プロセスを残さない**（process group 単位の kill。cgroup が使える場合は `cgroup.kill` も、§3.6）。

### 保証の検証（#10）

上の保証は、`crates/texrun-texlive/tests/` の integration test で、workspace の作成から engine の compile までを通して確認している。
fixture は `tests/fixtures/security/` にあり、防御が効いていることを確かめる最小限の内容だけを置く。
拒否を確かめる test では、同じ仕組みが許される場合（workspace 内への読み書きなど）の対照も実行し、fixture の誤りで「拒否された」ように見えることを防ぐ。
CI の `integration` job で毎回実行する（[development.md](development.md#tex-live-integration-test)）。

| 保証 | test |
| --- | --- |
| shell escape（`\write18` は `\immediate` あり・なしの両方、pipe による open）が実行されない | `security.rs`: `shell_escape_*` |
| 入力・workspace・HOME（`~/.latexmkrc`、`~/.config/latexmk/latexmkrc`）の latexmk rc が読まれない（workspace へのコピーでも除外される） | `security.rs`: `rc_files_in_the_project_are_not_copied_or_read` |
| 先頭行の format 指定が無視される | `security.rs`: `format_line_is_ignored`（対照: 先頭行を解釈させた pdflatex では同じ fixture が失敗する） |
| pdflatex・bibtex・makeindex が shell を経由せずに起動される | `security.rs`: `auxiliary_tools_are_started_without_a_shell`、`latexmk.rs`: `no_shell_between_latexmk_and_pdflatex` |
| `\input` / `\openin` / `\openout` で、絶対パスと `..` を含むパスが拒否される | `security.rs`: `input_*` / `openin_*` / `openout_*`（対照: `reading_inside_the_workspace_works` / `writing_inside_the_output_directory_works`） |
| root 外を指す symlink で workspace を作れない | `security.rs`: `symlink_outside_the_root_is_rejected_before_compiling` |
| timeout / cancel で子孫プロセスを残さない | `scenarios.rs`: `timeout`、`latexmk.rs`: `cancel_stops_the_whole_process_tree` |
| process group の kill・timeout・cancel・rlimit・leader の reap（TeX を使わない fake script） | `crates/texrun-process/tests/supervise.rs` |
| 出力の上限 | `latexmk.rs`: `output_directory_limit_stops_the_compile` / `per_file_limit_stops_the_compile` |
| CPU 時間・address space・cgroup の memory / プロセス数の上限で engine が止まり、結果が返る | `limits.rs`（TeX Live）、`crates/texrun-process/tests/limits.rs`（fake script） |
| 長い文書（200 ページ超、目次・相互参照・bibtex・makeindex）が既定の上限に当たらない | `limits.rs`: `a_long_document_stays_within_the_default_limits` |
| process group を抜けたプロセスも、cgroup があれば停止する | `crates/texrun-process/tests/limits.rs`: `cgroup_kill_reaches_a_process_outside_the_group` |

画像・bibtex の database・pdfTeX のファイル情報系 primitive の経路は、§6 の実験で確認したもので、fixture にはまだ含めていない。

### sandbox backend で追加で保証する（`--backend container`、§4）

in-process 実行の「保証する」1〜5 は、sandbox backend でもそのまま成り立つ（同じ rc・argv・env allowlist・kpathsea 設定・上限を container の中で適用する）。その上で、次のことを **OS（container の namespace と cgroup）で** 保証する。下の「保証しない」のうち、ここに挙げたものは sandbox backend では保証する側に移る。

1. **host の filesystem は、workspace 以外は見えない。**
   - container に mount するのは、workspace（read-only）、その中の output dir と engine の `HOME`（書き込み可）、texrun 管理 rc のディレクトリ（read-only）だけである。preview の container に mount するのは、PDF のコピー（read-only）と preview tool の作業ディレクトリ・`HOME`（書き込み可）だけである（§4「preview」）。
   - TeX Live の texmf ツリーは engine image のもので、root filesystem ごと read-only である。host の TeX Live、host のユーザーの設定・秘密情報、入力プロジェクトそのものは container の中に存在しない。
   - これは読み込み経路によらない。paranoid mode の検査が及ばない font 関連の読み込みや、PDF object にファイルを直接埋め込む pdfTeX の primitive でも、workspace と image の外は読めない。
   - workspace の入力ファイルも書き換えられない（書き込めるのは output dir と `HOME` だけ）。
2. **network は無い**（`--network none`。loopback だけ）。compile と preview は network 無しで行われる（#24）。container の中から外部の address への接続は、TCP / UDP、IPv4 / IPv6 とも `Network is unreachable` で即座に失敗し、名前解決もできないことを test で確認している（対照として、同じ probe で container 内の loopback には接続できることも確かめる）。
3. **engine は権限を持たない。** non-root user（texrun を起動したユーザーの uid / gid。texrun が root の場合は image の専用 user の 10001）で動き、capability は全て落とし（`--cap-drop ALL`）、`no-new-privileges` で setuid などによる権限の獲得もできない。root filesystem は read-only で、書き込めるのは `/tmp`（`noexec` の小さい tmpfs）と上の mount だけである。
4. **process tree 全体の memory・プロセス数・CPU を常に制限する**（container の cgroup。§3.10 の値。preview の container は preview tool の値）。委譲された cgroup の有無（in-process 実行の `--cgroup`）に関係なく、macOS でも（runtime の VM の中で）効く。runtime が上限や制限を黙って捨てた場合（kernel が対応していない場合など）は、container を起動せずに `EngineError::Unavailable` で失敗する（fail-closed。`create` の後に runtime の記録を確かめる、§4）。
5. **timeout / cancel で container を残さない。** container は texrun が名前と label を付けて作り、停止のたびに `rm --force` で消す。texrun 自身が強制終了（`SIGKILL` など）された場合は、container の中の `timeout` が compile の timeout + 45 秒で engine を止める（中の CPU 時間の上限が先に効くこともある）が、**停止した container と runtime の CLI のプロセスは残りうる**。label で見つけて消す（`docker ps -a --filter label=org.texrun.sandbox`、自動の回収は #49）。preview の最中だった場合は、temp dir の下の scratch directory（`texrun-preview-*`、mode 0700。PDF のコピーと描画途中の画像を含む）も残る（workspace の `texrun-ws-*` と同じ。#49 の回収の対象）。
6. **engine（pdfTeX と、それが使う画像ライブラリ）と preview tool（MuPDF / Poppler）の脆弱性の影響は、container の中に閉じる**（1〜3 の範囲。container runtime と kernel の境界を信頼する）。preview tool は engine が作った PDF を解析するので、その PDF は攻撃者の管理下にあるとみなす。preview tool は engine とは別の container（PDF のコピーだけが見える）で動き、host の preview tool は使わない（§4「preview」、#46）。

検証は次の test で行い、CI の `sandbox` job で毎回実行する（[development.md](development.md#container-backend-test)）。#10 の fixture（`scenarios.rs`、`security.rs`）も、`TEXRUN_TEST_BACKEND=container` で sandbox backend を使って全て実行する（host の TeX Live を直接使う 2 件を除く）。

| 保証 | test |
| --- | --- |
| 非 root、capability 無し、`no-new-privileges`、runtime の既定の seccomp profile（`Seccomp: 2`）、network は loopback だけ、root filesystem と workspace が read-only、output dir だけ書き込める、mount していない host のディレクトリが見えない、env が allowlist だけ、cgroup の `pids.max` / `memory.max` / `cpu.max`、rlimit | `crates/texrun-sandbox/tests/container.rs`（TeX を使わない `sh` の script） |
| timeout・cancel・drop で container が消える、container 内の deadline、OOM kill の記録 | 同上 |
| runtime の記録（`HostConfig`）に上限・制限が無い container は起動せずに消す。`create` が失敗したら名前で消す | `crates/texrun-sandbox/src/container.rs` の unit test（値の違う `inspect` を返す fake runtime） |
| PDF object への埋め込みでも workspace 外の host のファイルが読めない（絶対パス、`..`。ファイルを開けずに失敗したことも確認する。対照: workspace 内のファイルは埋め込め、host backend では同じ fixture で外のファイルに届く） | `crates/texrun-texlive/tests/container.rs`: `files_outside_the_workspace_cannot_be_embedded`、`the_host_backend_does_not_hide_the_host_from_file_embedding`（host backend、`integration` job） |
| texmf ツリーは image のもの | 同: `the_texmf_tree_is_the_images` |
| container 内でも CPU 時間・address space の上限で `resource_limit` になる、timeout / cancel で container が残らない | 同: `the_cpu_time_limit_applies_in_the_container` など |
| diagnostics の path が、mount 先（既定の `/workspace`、任意の path）によらず workspace 相対になる | 同: `diagnostics_are_workspace_relative_with_any_mount_point`、`apps/texrun/tests/container.rs` |
| container の中から network に出られない（`NetworkMode=none`、外部への TCP / UDP 接続が `Network is unreachable`、名前解決の失敗。compile の container と preview の container） | `crates/texrun-sandbox/tests/container.rs`: `a_container_cannot_reach_the_network`、`a_session_runs_programs_one_after_another_in_one_container` |
| preview の container: 制限と上限（rlimit は tool ごとに `prlimit`）、PDF のコピーが read-only、kill された run で container ごと消える、OOM kill の記録、container の寿命 | 同: `a_session_*`、`a_killed_run_stops_the_session`、`an_oom_kill_in_a_session_is_recorded` |
| preview の画像は container から見えないコピーを検査して保存し、保存後に元のファイルを書き換えても成果物は変わらない（別の inode） | `crates/texrun-preview/src/fsops.rs` の unit test: `a_staged_copy_is_what_is_stored` |
| preview tool を container で動かしても結果が host と同じ。出力先の symlink をたどらない、container と scratch を残さない、container が使えなければ host の tool に fallback しない | `crates/texrun-preview/tests/real_tools.rs`（`TEXRUN_TEST_BACKEND=container`）、`crates/texrun-preview/tests/container.rs`、`apps/texrun/tests/container.rs`: `previews_are_rendered_in_the_container` |

### 保証しない（MVP の in-process 実行の限界）

完全な対策は sandbox backend（§4、#26）で扱う。各項目の末尾に、sandbox backend での扱いを書く。

- **font 関連の読み込みは保証しない。** font・font map・encoding などの読み込みには、paranoid mode の検査が及ばない経路がある。sandbox backend では、host の filesystem が見えないことで保証する（上の 1）。
- **PDF object にファイルを直接埋め込む pdfTeX の primitive は保証しない。** この経路は paranoid mode の検査対象外で、workspace 外のファイルを読めることを確認した。sandbox backend では保証する（上の 1。test あり）。
- **TeX Live の texmf ツリー等、host の一部は名前で指定すれば読める。** paranoid mode の判定は「TeX に渡された名前」に対して行う。そのため、各ファイル種別の検索パスで見つかるファイルは読める。検索パスはファイル種別ごとに次のとおり。
  - TeX 入力（`\input` / `\openin`）: `TEXINPUTS`（texmf ツリーの `tex/` 以下と cwd）
  - 画像: `TEXINPUTS`
  - bibtex: `BIBINPUTS` / `BSTINPUTS`（`bibtex/` 以下）
  - font 関連: font 用の各パス（`fonts/` 以下と、OS の font ディレクトリ）

  たとえば `\openin` では、`tex/` 以下の `.sty` / `.cls` は読めた。一方、`web2c/` にある `texmf.cnf`、`ls-R`、font map、`.bst` は見つからなかった。これらのファイルは通常公開情報だが、host 固有の設定やローカルにインストールしたパッケージが含まれうる。sandbox backend では、読めるのは engine image の TeX Live（公開の Debian パッケージ）だけで、host のものは読めない。
- **OS レベルの隔離は無い。** engine は texrun を起動したユーザーの権限で動く。画像や PDF を解析するコード（pdfTeX・libpng・libjpeg・poppler / MuPDF など）にメモリ安全性の脆弱性があれば、細工した入力で任意コード実行されうる。その場合、そのユーザーが読み書きできるものはすべて危険にさらされる。sandbox backend では、engine（pdfTeX など）と preview tool（poppler / MuPDF）について、その影響を container の中に閉じる（上の 3・6）。
- **network は遮断していない**（§3.8、#24）。sandbox backend では compile も preview も遮断する（上の 2）。in-process 実行で遮断しない理由は §3.8 に書いた。
- **委譲された cgroup が使えない環境では、process tree 全体の memory とプロセス数を OS レベルで制限しない**（§3.10）。macOS、通常の Docker コンテナ、多くの Linux の端末 session がこれに当たる。sandbox backend の compile では、container の cgroup で常に制限する（上の 4）。
  - その場合に効くのは、プロセスごとの `RLIMIT_CPU`（Linux・macOS）と `RLIMIT_AS`（Linux のみ）である。macOS では memory を OS レベルで制限しない。
  - pdfTeX のメモリは `texmf.cnf` の固定容量（`main_memory` 等）で頭打ちになる。LuaTeX など他の engine はこの限りではない。
  - プロセス数は、TeX が shell escape 無しではプロセスを起動できないことと、texrun 管理 rc の latexmk が決まったツールだけを順に起動することに頼る。
- **workspace 内での書き込みは止めない。** TeX は output dir 配下に、任意の名前・任意の数のファイルを作れる。サイズは §3.2 の上限で抑えるが、ファイル数・inode は制限しない。
- **上限に達するまでの資源消費は起こりうる。** ログを出し続ける文書では、ログと stdout がそれぞれ約 32 MB/s の速さで増えた（§6）。
- **cgroup が使えない環境では、process group から抜けるプロセスは追えない。** sandbox backend では、container ごと消すので残らない（上の 5）。
  - 新しい session を作った子孫は `killpg` の対象外になる。cgroup が使える場合は `cgroup.kill` で止まる（§3.10）。
  - shell escape を無効にした TeX と、texrun 管理 rc の latexmk は、そのようなプロセスを起動しない。ただし OS として保証するものではない。そのようなプロセスが CPU を使い続けられる時間は `RLIMIT_CPU` で抑える（§3.10）。
- **生成物から host の情報が漏れる。** 詳細は §3.9。sandbox backend では、log・`.fls` などに出る path は container の中のもの（`/workspace`、image の texmf）になり、host の temp dir 名やユーザー名は出ない。PDF の日時と banner は同じである。
- **TeX / latexmk / kpathsea 自体のバグ** によって境界が破れる場合。sandbox backend では、破れても上の 1〜4 の範囲に留まる。
- **sandbox backend でも保証しないもの:**
  - container runtime・OCI runtime（runc など）・Linux kernel の脆弱性による container からの脱出。より強い隔離（gVisor の `runsc`、microVM）は §4 の比較に留める。
  - preview の container の中での、tool の run 同士の分離。preview の tool は 1 つの container の中で順に動く。侵害された tool が残したプロセスは、preview が終わって container ごと消えるまで動き続け、tool の作業ディレクトリ（`work`）の中身をいつでも書き換えうる。そのため texrun は、`work` の画像を検査も保存もしない: 画像を、container から見えない preview ディレクトリの texrun 専用のファイルに（残りのサイズ予算 + 1 byte を上限に）コピーし、**コピーの方を** その descriptor で検査して（PNG・寸法・サイズ）`renameat` する（§4「preview」）。保存した画像は container と inode を共有しないので、検査した内容がそのまま成果物になり、検査の後の書き換えは届かない。影響は、後のページの描画結果（画像の中身）に留まる（PDF は同じ攻撃者の管理下にあるので、新たに得るものは無い）。
    - tool の run ごとに残ったプロセスを止める（`exec` のたびに container の中のプロセスを kill する）ことも検討したが、行っていない。container の main process（`sleep`）も tool と同じ uid で動くので、texrun が中から uid 単位で kill すると container ごと止まり、区別して kill するには run ごとにもう 1 回 `exec` が要る（1 ページあたり約 0.03 秒の追加）。上のコピーで host 側の保証は成り立つので、残ったプロセスは preview の終了時に container ごと消すことにした。
  - workspace と output dir の中身は container から読める・書ける（in-process 実行と同じ。入力は read-only だが、書き込める場所は §3.2 のサイズ上限だけで、ファイル数は制限しない）。
  - プロセス数の上限（`pids.max`）に達したことの判定。container の cgroup の event を texrun は読まないので、fork の失敗を perl が再試行し続けた場合は timeout として報告される（memory の上限は OOM kill の記録で判定する。#49）。
  - rootless mode の Docker（`dockerd-rootless`）は想定していない（#47）。container の uid が host の subordinate uid に写るので、output dir に書けずに compile が失敗する（安全側には倒れる）。
  - macOS では container は runtime の VM の中で動く。workspace は VM と共有された host の temp dir にある（Docker Desktop / OrbStack の既定の共有範囲）。
- **表示上の偽装。** `WorkspacePath` は bidi 制御文字（U+202A〜U+202E、U+2066〜U+2069）とゼロ幅文字（U+200B〜U+200F、U+FEFF）を許容している。
  - 人間向けの出力でこれらを含む path を表示するときは、escape する（#6）。
  - JSON 出力はそのまま出す。JSON 文字列としては正しく、扱いは消費側の責任とする。
  - output dir と entrypoint の名前では、これらの文字を §3.5 の検査で拒否する。

## 3. 決定事項

各項目の末尾に実装担当 Issue を記す。値は MVP の定数である。CLI（#6）で変えられるのは compile の timeout（`--timeout`）と preview のページ範囲・DPI（`--pages` / `--preview-dpi`、上限は下表のまま）、cgroup を使うかどうか（`--cgroup`、§3.10）だけで、それ以外の上限は CLI からは変更できない。

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
| 子プロセスが書く 1 ファイルの最大サイズ | **256 MiB** | `RLIMIT_FSIZE`（下記。exec gate では Linux・macOS、rc の stdin gate では Linux）。latexmk と全子孫に継承させる。全 OS: output dir の合計サイズと同じ poll で、最大のファイルが上限に達したら process group を kill する |
| output dir の合計サイズ | **1 GiB** | timeout の poll ループ内で定期的に（目安 500 ms ごと）集計し、超えたら process group を kill する |
| stdout / stderr の保持量 | **各 4 MiB**（先頭を保持し、超過分は読み捨てる） | reader thread が pipe を最後まで読み続け、保持する量だけを制限する。pipe を読まずに止めると engine が block する |
| PDF artifact | 256 MiB（`RLIMIT_FSIZE` と同じ値で自動的に頭打ちになる） | — |
| preview のページ数 | 既定は **先頭 20 ページ**。範囲を指定した場合（`--pages`）も **最大 200 ページ** | #8 |
| preview 画像の合計サイズ | **128 MiB**。超えた時点で以降のページを生成せず、warning を出す | #8 |
| preview 画像 1 枚の長辺 | **4096 px**。超えるページは DPI を下げて描画し、info を出す | #8。PDF が宣言するページサイズ（信頼できない値）の parse に依存しないよう、3 段で強制する（下記）。1 ページあたりの出力と、Linux 以外でのメモリを抑える唯一の手段 |

- `RLIMIT_FSIZE` を超えて書き込もうとすると、engine は `SIGXFSZ` で終了する。20 MiB に制限してログを出し続けさせたところ、ログはちょうど 20 MiB で止まり、latexmk は失敗終了した。
- latexmk の rlimit（`RLIMIT_FSIZE` と §3.10 の `RLIMIT_CPU` / `RLIMIT_AS`）の設定方法:
  - 子プロセスの `exec` 前に `setrlimit` する `pre_exec` は `unsafe` で、この workspace は `unsafe_code = "forbid"` なので使わない。
  - **exec gate が使える場合は、latexmk も exec gate（下記、preview tool と同じもの）経由で起動する**（#25。`LatexmkConfig::with_exec_gate`。CLI は常に指定する）。gate は latexmk（perl）の最初の命令より前に limit を設定するので、Linux でも macOS でも、latexmk とその全子孫が最初から制限される。gate は stdin を `/dev/null` にするので、このとき rc には開始の合図の待機を入れない。
  - exec gate が使えない場合（library として gate を指定しない、`/proc` の無い Linux で CLI の gate が見つからない、など）、Linux では従来の方法にする。spawn 直後に親から `prlimit(2)` で latexmk に設定し、latexmk が設定前にファイルを書いたり子プロセスを起動したりしないよう、texrun 管理 rc（§3.5）の先頭で stdin から開始の合図を待たせる。親は `prlimit` の後に合図を送る。合図が来ずに stdin が閉じた場合、rc は何もせずに終了する（終了コード 125）。rc の待機は perl が rc を読むまで始まらないが、その前に perl が書くファイルや起動する子プロセスは無いので、latexmk がファイルを書く前・子を起動する前に limit が掛かる点で exec gate と同じ保証である。
  - `prlimit` の適用と開始の合図の送信は `texrun-process` が行う（`Rlimits` と `StartMode::StdinGate`）。合図の内容と、合図を待つ rc は engine（`texrun-texlive`）側に置く。
  - 事前の確認の後で exec gate が使えなくなった場合（確認と spawn の間に binary が消えた、など）、engine は何も起動しない（gate を必須として渡すので、supervisor は `RunError::Unsupported` を返す）。Linux では rc の stdin gate でやり直す。macOS では、gate が必須なら `EngineError::Unsupported`（CLI では exit 3）、そうでなければ次の場合と同じになる。
  - exec gate が無く `prlimit` も無い場合（macOS で library として gate を指定しない）は、rlimit を設定しない。1 ファイルの上限は poll（目安 500 ms ごと）でのみ強制するので、検出までの間は上限を超えて書かれうる（ログを出し続ける文書で約 16 MB）。結果の `resource_limits.rlimits` が `false` になり、`notes` に理由が入る。
- `RLIMIT_FSIZE` で書き込みが止まった場合も、終了後の集計で上限に達したファイルを検出し、同じ diagnostic を付ける。texrun 管理 rc は、起動したツールが `SIGXFSZ`（または `SIGXCPU`）で終わったら、latexmk をすぐに終了コード 128 + signal で終わらせる（§3.10）。
- 設定する値は、texrun 自身の limit（子プロセスが継承する値）と上限値の小さい方とする。soft limit は texrun 自身の soft limit と要求値の小さい方、hard limit は texrun 自身の hard limit と要求値の小さい方にする（#25 で変更。以前は soft にも hard との min を設定していたので、利用者が `ulimit -S` で下げた soft limit より大きくなりえた。#44 の nit 5）。上限を強める方向にだけ働くので、権限は要らない。
- **core dump は無効にする**（`RLIMIT_CORE=0`、soft・hard とも。`RLIMIT_FSIZE` / `RLIMIT_CPU` と同時に設定する）。`SIGXFSZ` と `SIGXCPU` の既定の動作は core dump で、core ファイルは TeX の cwd（workspace 内）に作られ、output dir の集計の対象外になるためである。`texrun-process` は、`Resource::FileSize` か `Resource::Cpu` を soft 0 の `Resource::Core` なしで指定した spec を、spawn の前に `RunError::InvalidSpec` で拒否する。rlimit を設定しない場合（上記）は、停止は `SIGKILL` で行うので、core は作られない。
- 上限によって停止した場合は `CompileOutcome::Failed` とし、texrun 由来の diagnostic（「output limit exceeded」等）を付ける。diagnostic の kind は `resource_limit`（#25 で追加。以前は `other`）で、§3.10 の CPU 時間・memory・プロセス数の上限と共通である。専用の outcome は追加しない（§3.10）。
- preview（#8）の長辺の上限は、次の 3 段で強制する。
  1. ページサイズから DPI を下げる。`mutool` はページの拡大係数（`UserUnit`）を反映して描画するので、係数を掛けたサイズで計算する。
  2. `mutool draw` には、上限を bounding box（`-w` / `-h`）としても渡す。縮小だけに効き、小さいページは拡大しない。`pdftoppm` には拡大を伴わずに上限を渡す option が無いので、この段は無い。
  3. 描画後に PNG の IHDR の寸法を確認し、上限（丸め分 2 px を許容）を超えた画像は捨てて `render_failed` にする。
- preview tool の資源制限は、**exec gate** で tool の exec 前に設定する（#41。`texrun-process` の `StartMode::ExecGate`）。
  - gate は texrun 自身の隠しサブコマンド（`texrun __exec-gate`。clap の解析より前に分岐し、`--help` にも出ない）である。supervisor は tool の代わりに gate を spawn する。gate は次の順に処理する。
    1. 引数を検査する。protocol の版、`--rlimit <名前>=<値>` の並び、`--` の後に tool の絶対パスと引数、の形以外は拒否し、何も実行しない。
    2. stdin（supervisor との Unix socket pair）で開始の合図を待つ。supervisor は `Launcher::on_spawn` の後に合図を送る。合図が来ずに閉じた場合は、何も実行せずに終了する（終了コード 125）。
    3. `setrlimit(2)` で自分に limit を設定する（texrun 自身の soft・hard limit を超えない）。失敗したら exec しない。
    4. stdin を `/dev/null` に差し替え、supervisor に `ok` を報告してから、tool を `exec` する。報告用の fd は close-on-exec なので、exec が成功すると閉じる。exec の失敗（tool が無いなど）も報告され、supervisor は gate を使わない場合と同じ spawn error にする。
  - rlimit は exec の後も引き継がれ、PID（= PGID）も変わらない。そのため tool は最初の命令から制限された状態で動き、tool が起動する子孫もすべて limit を継承する。process group の kill と reap の前提（§3.6）もそのまま成り立つ。
  - gate は tool の引数や PDF のパスを解釈しない。`--` の後はそのまま `exec` の argv に渡し、shell で解釈させることも、`PATH` を検索することもない（tool は絶対パスで指定する）。なお `exec` は C library の `execvp` を経由するので、実行形式として認識されない file（ENOEXEC）は `/bin/sh` で実行し直される。これは gate を使わない spawn（std）と同じ挙動で、tool は texrun が検出した実行ファイルである。環境変数と cwd は、supervisor が gate に与えたもの（§3.4 の allowlist、`work/` の fd）をそのまま引き継ぐ。
  - tool が受け取る fd は、stdin（`/dev/null`）・stdout・stderr だけである。gate 自身の fd（socket pair と報告用の fd）は close-on-exec なので継承されない。**既知の制約**: texrun を起動した親から close-on-exec でない fd を継承していた場合、その fd は gate を経て tool にも継承される（gate を使わない spawn と同じ）。自分が所有しない fd を閉じるには `unsafe` が要るので、gate は fd を整理しない。
  - gate が報告を書くのは、stdin が socket の場合（supervisor が用意したもの）だけである。利用者が手で起動した場合に、端末や stdin の file へ書き込むことはない。合図の送信は `SIGPIPE` を起こさない（Linux は `MSG_NOSIGNAL`、macOS は `SO_NOSIGPIPE`）。
  - `unsafe`（`pre_exec`）は使わない。`setrlimit` も `exec` も safe な API である。
  - 設定する値:
    - `RLIMIT_AS`: 2 GiB（Linux のみ。macOS は既に確保済みの address space より小さい値を拒否し、強制もしないので設定しない）
    - `RLIMIT_FSIZE`: 残りの画像予算 + 1 byte。ただし 16 MiB 未満にはしない（`HOME` に fontconfig の cache などを書くため）
    - `RLIMIT_CORE`: 0
    - `RLIMIT_CPU`: preview の timeout + 10 s（soft）、その 5 s 後（hard）（§3.10）
  - `setrlimit` は macOS にもあるので、macOS でも `RLIMIT_FSIZE`・`RLIMIT_CORE`・`RLIMIT_CPU` が exec 前から効く。
  - cgroup（§3.10）への移動は、gate が合図を待っている間（手順 2）に supervisor が行う。exec 前なので、全ての子孫が対象になる。
  - gate を使えない場合（`ExecGate` の path が実行可能な file でない、など）の動作は、次のとおりである。
    - `ExecGate::with_required(true)` か `Spec::require_rlimits` を指定した場合は、何も spawn せずに `RunError::Unsupported` にする。
    - それ以外の場合は `StartMode::Immediate`（spawn 直後に親から `prlimit(2)`、Linux のみ）に fallback し、`Finished::gate_fallback` に理由を記録する。`Immediate` の保証範囲（設定までの間隔は時間で抑えられない、その間の確保や書き込みは取り消されない、その間に起動された子孫は制限されない）は `StartMode::Immediate` の rustdoc に書いてある。
    - preview は、gate が使えないことを最初に `ExecGate::check` で確かめ、warning の notice（`resource_limits`。CLI の JSON にも出る）で報告する。gate が必須なら、tool を 1 つも動かさずに preview を skip する（status は `skipped`）。必須でなければ、上記の fallback で描画する。実行中に gate が使えなくなった場合も、同じ notice を 1 回だけ出す。事前の確認の後で必須の gate が使えなくなった場合（supervisor が spawn 前に `RunError::Unsupported` を返す）も、その tool を動かさずに `resource_limits` の notice にして、preview をそこで止める（#25。以前は tool の失敗の notice `tool_unavailable` だった）。
    - **CLI は gate を必須にする（fail-closed）**。preview が失敗しても compile の結果や exit code は変わらない（§3.2 の preview の方針）ので、制限を弱めて描画するより、描画しない方を選ぶ。
    - CLI の gate は、Linux では `/proc/self/exe` である。spawn された子（exec 前の texrun 自身）の中で解決されるので、実行中に binary が削除・置き換えされても（package の更新など）、いま動いている texrun と同じ image が gate になる。host で起動する子に限って成り立つ（将来の container launcher、#26 では使わない）。macOS では `current_exe()` を使い、その path が使えない場合は上記のとおり preview を skip して notice を出す。
    - `/proc` が mount されていない Linux（一部の sandbox や chroot）では、`/proc/self/exe` の確認が常に失敗するので、**CLI の preview は常に skip になる**（`resource_limits` の notice）。fail-closed なので正しい挙動である。compile は rc の stdin gate（`prlimit`）で limit を掛けて続ける（上記）。
    - library として gate を指定せずに `Previewer` を使った場合は、`Immediate` になる。
  - library として使う場合、gate の実行ファイルは呼び出し側が明示する（`ExecGate::new`。自分の binary の隠しサブコマンドで `texrun_process::run_gate` を呼ぶか、`texrun-process` の `texrun-exec-gate` binary を使う）。環境変数や `PATH` からは探さない。
  - latexmk も同じ exec gate で起動する（#25、上記の「latexmk の rlimit の設定方法」）。engine には `LatexmkConfig::with_exec_gate` で gate を渡す。
- preview 画像は private な scratch dir に描画する。検査の後、`preview/` へ移す。scratch dir は、host の preview tool では output root の中に、container の preview tool（`--backend container`）では output root の外（temp dir）に作り、container では画像をコピーしてから検査する（§4「preview」）。以下は host の場合である。
  - scratch dir（`.texrun-preview-*`、mode 0700）とその下の `home/`・`work/` は、output root の fd から `mkdirat` で作り、`openat(O_NOFOLLOW)` で開く。後片付けも、開いた fd から `unlinkat` で行い、symlink は辿らない。
  - tool の cwd は、開いた `work/` の fd を使う（Linux では `/proc/self/fd` 経由で、子プロセスがその fd 自体に `chdir` する）。`/proc` が無い環境（macOS）では、path が同じ directory（dev / inode）を指すことを確認してから path で起動する。この確認と `chdir` の間は atomic ではない。
  - tool は `work/` 内の相対名に書き出し、texrun はそれを `work/` の fd から検査・移動する。`HOME` だけは環境変数なので path で渡す（cache の置き場で、texrun は中身を読まない）。
  - 移動は `renameat` で、directory の fd 間で行う。
  - `preview/` は `mkdirat` で作り、`openat(O_NOFOLLOW)` で 1 component ずつ開く。
  - 同じ名前の既存 file や symlink は置き換える。symlink の先には書き込まない。
  - これは #21 の artifact 収集と同じ水準である。compile から残ったプロセスが directory を symlink に差し替えても、画像の書き込み先は output root の外に出ない。

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
  - `WorkspaceConfig::excluded_paths` の root 相対パス（CLI は project 内の出力先を深さによらずここに加える。入力上限を前回の出力で消費させないため）。名前と同じく component ごとに folding と workspace FS での別名チェックを行う
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
- allowlist の適用（`env_clear()` の後に設定）と `PATH` の相対 entry の除去は、`texrun-process` の `EnvAllowlist` / `sanitize_path` で行う。値は engine / preview がそれぞれ決める。
- preview tool（`mutool` / `pdfinfo` / `pdftoppm`、#8）には `PATH`・`LC_ALL=C`・`HOME`（preview ごとに作る空の一時ディレクトリ）だけを渡す。kpathsea の変数は不要なので渡さない。tool は検出時に解決した絶対パスで起動する。`PATH` の相対 entry（`.` など）は、検出に使わず、tool に渡す `PATH` からも除く。

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
  - bibtex を使う文書では、latexmk の stderr に `Kpsewhich command needed but not set up` が出る。diagnostics は main の `.log` と BibTeX の `.blg`、および latexmk の BibTeX に関する決まった行（`.bib` が無いときの veto、BibTeX の error の要約）からだけ作るので、この行は diagnostics にならない。
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

latexmk と preview tool は、どちらも `crates/texrun-process` の supervisor（`texrun_process::run`）で起動・停止する（#32）。以下はその共通の動作である。

- spawn 時に新しい process group を作る（`CommandExt::process_group(0)`）。
- timeout / cancel（`CancelToken`）/ 出力上限の超過時は、**process group に `SIGKILL` を送る**（`killpg`）。
  - latexmk だけに `SIGTERM` を送ると、その子プロセスが親 PID 1 のまま走り続けることを確認した。
  - TeX に graceful shutdown は不要で、途中までの log はファイルに残る。そのため `SIGTERM` による猶予は設けない。
- 子プロセスが正常終了した後も `killpg(SIGKILL)` を 1 回送り、残った子孫を掃除する（`ESRCH` は無視する）。
  - **leader を reap する前に `killpg` する。** leader の終了は `waitid(P_PID, WEXITED | WNOHANG | WNOWAIT)` で検知し、reap しない。leader の zombie が PID と PGID を確保しているので、`killpg` が無関係な process group に届くことは無い。
  - `killpg` の後で leader を reap する。timeout / cancel / 上限超過の場合も同じ順序で行う。
  - 途中で error や panic が起きた場合も、guard（`Drop`）が同じ順序で kill と reap を行う。
- run が cgroup を持つ場合（§3.10）は、`killpg` のたびに `cgroup.kill` にも書く。process group から抜けたプロセス（新しい session を作った子孫など）も、これで止まる。leader の reap の後、cgroup が空になるのを最大 2 秒待ち、event を読んでから削除する。
- cancel、timeout / deadline、出力サイズ（呼び出し側の check hook）は、同じ poll ループで確認する（#20 からの申し送りどおり）。
  - poll 間隔は全 program 共通で 10 ms（`texrun_process::POLL_INTERVAL`）。`waitid` は軽く、preview は短い tool を最大数百回起動するので、短い方に揃えた。
  - 重い check は、呼び出し側が間隔を指定する（latexmk の output dir の集計は 500 ms ごと、preview の画像 1 枚の `fstatat` は毎回）。
- stdout / stderr の reader thread は、process group を kill した後に最大 500 ms（`texrun_process::READER_GRACE`）だけ待つ。group から抜けたプロセスが pipe を開いたままでも、compile や preview は終わる（それまでに読めた分を返す）。全ての書き手が kill された後の pipe はすぐ EOF になるので、通常は待たない。
- `EINTR` は `waitid` と pipe の read で retry する。
- 将来の拡張（#26 の container runtime）は、`texrun_process::Launcher`（起動方法、spawn 直後・kill 時・reap 後の hook。どれも leader の PID を受け取る）を拡張して行う。hook の形は #26 で見直す。
  - #25 の cgroup は `Launcher` にせず、supervisor 自身が扱う（`Spec::cgroup`）。cgroup の kill は全ての group kill と一緒に行う必要があり、cgroup の event（OOM kill など）は結果（`Finished::cgroup`）に載せる必要があるためである。`Launcher::apply_rlimits() == false` の launcher（container runtime が limit を扱う場合）では、cgroup も使わない。
  - `StartMode::Immediate` と組み合わせた場合、spawn 直後の hook と cgroup への移動より前に起動された子孫は、その対象から漏れる。全ての子孫を含めるには、stdin gate か exec gate（§3.2）と組み合わせる。どちらも hook と移動の間は子が合図を待っている。
- Linux 以外では `prlimit` が無いので、exec gate を使わない場合、rlimit は既定では適用せずに実行し、結果（`Finished::rlimits_applied`）に記録する。呼び出し側は `Spec::require_rlimits` で、適用できない場合に実行せず `RunError::Unsupported` にすることを選べる（latexmk の stdin gate はこれを指定する）。
- stdin gate の合図は poll ループの前に同期的に書くので、長さを 512 byte（POSIX の最小 `PIPE_BUF`）までに制限する。

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
- sandbox backend（`--backend container`、#26）の compile と preview（#46）は、常に network 無し（`--network none`）で動く。option で有効にする手段は設けない。runtime が network mode を `none` 以外で記録した container は起動しない（§4 の `HostConfig` の照合）。test は §2 の表。
- **in-process 実行（`--backend host`）では network を遮断しない**（#24 で判断した）。network を遮断したい場合（信頼できない文書）は `--backend container` を使う。理由:
  - **環境によって使えたり使えなかったりする。** Linux で権限なしに network を切る手段は、unprivileged user namespace の中に network namespace を作ることだけである。しかし、これを禁止・制限している環境が多い。Docker の既定の seccomp profile は `unshare` での user namespace の作成を拒否する（dev コンテナや多くの CI の container がこれに当たる）。Ubuntu 23.10 以降は AppArmor で unprivileged user namespace を制限し、Debian の古い kernel や一部のディストリビューションは sysctl で無効にしている。macOS には同等の手段が無い（`sandbox-exec` は非推奨）。使えない環境で fail-closed にすると texrun がほとんどの環境で動かなくなり、fail-open にすると「遮断されているか」が環境次第になって、保証として書けない。
  - **効果が限られる。** shell escape を無効にした TeX には network に接続する手段が無い（§3.5）。network が問題になるのは、engine や preview tool がメモリ安全性の脆弱性で侵害された場合である。in-process 実行ではその時点でユーザーの権限で filesystem 全体を読み書きできる（§2「保証しない」）ので、network だけを切っても、秘密情報の読み取りや、後でユーザーが実行するファイル（shell の設定など）の書き換えは防げない。境界として意味を持たせるには、filesystem と権限の隔離も同時に要り、それは sandbox backend で行っている。
  - **攻撃面が増える。** user namespace は kernel の権限昇格の脆弱性の主要な入り口の一つで、それを避けるために上のように制限されている。texrun のために有効にすることを利用者に求めない。
- package の自動インストールなど network を必要とする機能は提供しない方針を維持するので、どちらの backend でも、network が無いことで compile や preview が失敗することは無い。

### 3.9 生成物に含まれる host の情報（#4 / #5 / #6）

生成物には、次の host の情報が含まれる。

| 生成物 | 含まれる情報 |
| --- | --- |
| `.fls` / `.fdb_latexmk` | workspace の絶対パス（`PWD` 行。host の temp dir 名。macOS では user ごとのパスを含む） |
| `.log` | workspace と texmf ツリーの絶対パス |
| PDF | 作成日時（`CreationDate` / `ModDate`）、それに依存する `/ID`、pdfTeX と TeX Live の版を表す banner（`PTEX.Fullbanner`、`Producer`） |

これらの扱いは次のとおりとする。

- 収集する artifact は engine が報告したもの（PDF、log、preview）だけにする。`.fls` / `.fdb_latexmk` / `.aux` は収集しない（#4 / #5）。
- BibTeX の `.blg` も artifact にしない（#27）。内容は diagnostics（`bibtex_error` / `bibtex_failed` / `missing_file` など）の `raw_excerpt` に入る。engine は output dir 以下の `.blg` を、compile の timeout の外で読むので、読む量に上限を設ける。
  - output dir の走査: 最大 20,000 entry・深さ 16、symlink はたどらない
  - 読む `.blg`: この compile で更新されたもの（mtime が latexmk の開始以降）を最大 16 個。通常ファイルだけを `O_NOFOLLOW | O_NONBLOCK` で開き、先頭 1 MiB まで読む
  - `<stem>.blg` を先に読み、全 `.blg` を合わせて最大 200 個の diagnostics（各 `.blg` の要約と省略の通知は別）。parse は読んだ量に線形
  - 文書は output dir に任意のファイルを書けるので、`.blg` の内容も main の `.log` と同じく文書が制御できる入力として扱う。file を付けるのは entrypoint のディレクトリに実在する `.bib` / `.bst` だけなので、`.blg` を偽造しても workspace 外や存在しないファイルを指す diagnostic にはならない
  - output dir は compile ごとに新しい前提（CLI は毎回新しい workspace を作る）。mtime の判定は保険
  - `.bib` / `.bst` の file は、`.blg` が database / style として名前を挙げ、entrypoint のディレクトリ（latexmk が BibTeX の `BIBINPUTS` / `BSTINPUTS` の先頭に置く）に workspace 内の通常ファイルとしてあるときだけ付ける。`.aux` 内の位置や installed な style には付けない
- 既定では `SOURCE_DATE_EPOCH` / `FORCE_SOURCE_DATE` を設定しないので、PDF の日時は compile した時刻になる。再現可能なビルドを求められた場合に限り、CLI の option（#6）で `SOURCE_DATE_EPOCH=<値>` と `FORCE_SOURCE_DATE=1` を env allowlist に加える。これで、PDF の日時が固定されることを確認した。
- banner と log 内のパスは、MVP では抑制しない。生成物を第三者と共有する場合は、利用者の判断に委ねる。

### 3.10 CPU・memory・プロセス数の上限（#25）

engine（latexmk とその子孫の pdflatex / bibtex / makeindex）と preview tool（mutool / pdfinfo / pdftoppm）に、次の上限を掛ける。
層は 2 つある。プロセスごとの rlimit は常に掛け、process tree 全体の cgroup は、Linux で委譲された cgroup が使える場合だけ重ねる。

| 対象 | 既定値 | 方法 | 範囲 |
| --- | --- | --- | --- |
| CPU 時間（engine の各プロセス） | soft = compile の timeout + **10 s**（既定 70 s）、hard = soft + 5 s | `RLIMIT_CPU` | プロセスごと。Linux・macOS |
| CPU 時間（preview tool の各プロセス） | soft = preview の timeout + **10 s**（既定 40 s）、hard = soft + 5 s | `RLIMIT_CPU` | 同上 |
| address space（engine の各プロセス） | **4 GiB** | `RLIMIT_AS` | プロセスごと。Linux のみ（macOS は §3.2 のとおり設定できない） |
| address space（preview tool の各プロセス） | 2 GiB（#8 のまま） | `RLIMIT_AS` | 同上 |
| memory（engine 全体） | **4 GiB**（page cache を含む。swap 0） | cgroup `memory.max`、`memory.swap.max = 0`、`memory.oom.group = 1` | latexmk と全子孫の合計。Linux で cgroup が使える場合 |
| memory（preview tool 1 回） | **2 GiB** | 同上 | tool 1 回のプロセス全体。同上 |
| プロセス数 + thread 数 | engine **64**、preview tool **32** | cgroup `pids.max` | 同上 |
| CPU の同時使用 | **2 CPU** | cgroup `cpu.max`（`200000 100000`） | 同上 |
| プロセス数（UID 単位） | **使わない** | （`RLIMIT_NPROC`） | — |

値は `texrun_texlive::Limits`（engine）と `texrun-preview` の定数で、CLI からは変えられない（`--timeout` を伸ばすと CPU 時間の上限も伸びる）。

sandbox backend（§4）では、engine と preview tool の同じ値を container runtime に渡す（preview は §4「preview」）。CPU 時間・1 ファイルのサイズ・core は `--ulimit`、address space は container の中の `prlimit`（runtime の `--ulimit` が `as` を受け付けないため。latexmk の起動前に設定する）、memory・プロセス数・CPU は container の cgroup（`--memory` = `--memory-swap`、`--pids-limit`、`--cpus`）である。どれも latexmk の起動前に掛かり、`resource_limits` は `{"rlimits": true, "cgroup": true}` になる。memory の上限に達した場合は、runtime が記録する OOM kill（`State.OOMKilled`）で `resource_limit` にする。

**既定値の根拠（実測）。** 開発用コンテナ（§6 と同じ image、arm64、14 CPU）で、次の文書と tool を cgroup の中で動かし、`memory.peak` / `pids.peak` / `cpu.stat` を読んだ。`RLIMIT_AS` は `ulimit -v` で値を変えて成否を見た。

| 対象 | 結果 |
| --- | --- |
| 417 ページの文書（40 章 × 4 節 × 7 段落、数式 1,120 個、図 120 個（4724×3543 の PNG、JPEG、PDF）、目次・図目次・相互参照・hyperref・bibtex・makeindex）の latexmk 全体 | wall 3.3 s、CPU 3.3 s、memory.peak 47 MiB、pids.peak 10 |
| 同じ文書の pdflatex 1 pass | CPU 0.66 s、memory.peak 24 MiB。`RLIMIT_AS` 128 MiB で成功、64 MiB で失敗 |
| 7874×5906 の RGBA PNG（pdfTeX が展開する）を 2 回含む文書の pdflatex | memory.peak 68 MiB。`RLIMIT_AS` 256 MiB で成功、128 MiB で失敗 |
| 15000×11251 の RGBA PNG を含む文書の pdflatex | `RLIMIT_AS` 256 MiB で `fatal: memory exhausted (xmalloc of 168765000 bytes)`、512 MiB で成功。pdfTeX は alpha の平面（幅 × 高さ byte）を一度に確保するので、必要な address space は画素数に比例する |
| 1 ページの文書 | pdflatex は `RLIMIT_AS` 96 MiB 以下では起動しない（`texmf.cnf` の配列を最初に確保するため）。latexmk（perl）は 32 MiB で動いた |
| bibtex / makeindex | memory.peak 4 MiB / 1 MiB、CPU 10 ms 未満 |
| mutool draw（150 dpi、20 ページ）/ pdftoppm（同） | CPU 0.46 s / 1.76 s、memory.peak 30 MiB / 15 MiB。300 dpi でも `RLIMIT_AS` 256 MiB で成功 |

- address space と memory の 4 GiB は、実測の最大（pdflatex の `RLIMIT_AS` 256 MiB、memory.peak 68 MiB）に対して 16 倍以上の余裕がある。画素数で言えば、alpha 付きの画像は約 40 億画素（約 65000×65000）まで入る。大きな画像を含む文書でも誤爆しない値として、2 GiB（preview tool）より大きくした。
- `pids.max` の 64 は、実測の 10 に対して十分に大きい。pids は thread も数える。
- `cpu.max` の 2 CPU は、latexmk が pdflatex などを順に（同時には 1 つずつ）起動するので、通常の compile を遅くしない。複数のプロセスや thread で CPU を使い続ける場合に、host の CPU を占有させない。
- **CPU 時間は timeout から決める。** `RLIMIT_CPU` はプロセスごとの CPU 時間なので、single thread のプロセスは wall-clock 以上の CPU 時間を使えない。soft を timeout + 10 s にすると、timeout より前に通常の文書で当たることはない（上の 417 ページの文書は 1 pass 0.66 s）。この上限の役割は、(1) 複数 thread で CPU を使うプロセス、(2) process group を抜けて texrun の kill から逃れたプロセス（cgroup が無い環境）が CPU を使い続けられる時間を抑えることである。
- **soft と hard を分ける**（#44 の nit 5 の見送り分をここで決めた）。Linux は soft と hard が等しいと、`SIGXCPU` を送らずにすぐ `SIGKILL` する。`SIGXCPU` なら texrun が原因を判定できるので、soft で `SIGXCPU` を受け、`SIGXCPU` を無視するプロセスは hard（5 s 後）の `SIGKILL` で止める。§3.2 のとおり、soft は texrun 自身の soft limit も超えない。
- **`RLIMIT_NPROC` は使わない。** 実 UID ごとに、host 上のそのユーザーの全プロセスを数えるためである。開発機（macOS）では、そのユーザーのプロセスが 1,241 個あった。上限は「その時点の数 + 余裕」としか決められず、ブラウザなど他のアプリがプロセスを増やすと engine の fork が失敗する。root には効かない（dev コンテナは root で動く）。代わりに、cgroup の `pids.max` で process tree 単位に数える。cgroup が無い環境では、プロセス数は §2 のとおり OS では制限しない。`texrun_process::Resource::Processes` は library の利用者向けに残す。

**latexmk の起動（exec gate への統一）。** CLI は latexmk も preview tool と同じ exec gate（`texrun __exec-gate`）で起動する（§3.2）。これで macOS でも latexmk とその子孫に `RLIMIT_FSIZE` / `RLIMIT_CPU` / `RLIMIT_CORE` が最初から掛かる。exec gate が使えない場合の Linux の fallback として、rc の stdin gate を残す（§3.2）。engine には `LatexmkConfig::with_exec_gate(ExecGate)` で gate を渡す。library の利用者は、自分の binary の隠しサブコマンドで `texrun_process::run_gate` を呼ぶか、`texrun-process` の `texrun-exec-gate` binary を指定する（§3.2 の preview と同じ）。

**cgroup（Linux、cgroup v2）。**

- 使う cgroup は `Cgroups::detect` で探す。次の 2 か所だけを、この順に見る。
  1. **texrun 自身の cgroup。** texrun に明示的に委譲されていて（例: `systemd-run --user --scope -p Delegate=yes texrun compile ...`）、ほかのプロセスがいない場合に使う。委譲されていることは、systemd の委譲の印（xattr の `trusted.delegate` か `user.delegate` が `1`。systemd 251 以降が `Delegate=yes` の cgroup に付ける）で判断する。次のものは委譲の証拠にしない。
     - 書き込めること: root はどの cgroup にも書き込めるので、systemd が唯一の書き手として管理する cgroup（`Delegate=` の無い service など）を変更してしまう。
     - 所有者が texrun の（root でない）ユーザーであること: `user@<uid>.service` の下の cgroup（端末の app scope など）は全てそのユーザーの所有だが、`systemd --user` が管理しており、それ自身がさらに委譲した cgroup にしか印が付かない。

     印の無い古い systemd などで使わせたい場合は、library の `Cgroups::at(dir)` で明示する。

     cgroup v2 は、プロセスのいない cgroup からしか子 cgroup に controller を渡せない。そのため texrun は、まず自分を leaf の子 cgroup（`texrun-<pid>.main`）に移し、それから `memory` / `pids` / `cpu` を有効にする（systemd が委譲先に勧めている方法）。leaf は texrun の終了後に空のまま残り、委譲元（systemd の scope など）が消すときに一緒に消える。
  2. **cgroup namespace の root。** host の root cgroup ではなく（container の中）、cgroup v2 が `nsdelegate` 付きで mount されていて（kernel が namespace を委譲の境界として扱う。container の runtime が container に渡した cgroup である）、書き込めて、root 自身にプロセスがいない場合に使う（例: container のプロセスを leaf に移した場合。`docker/dev/with-cgroup.sh`）。
  
  これより上の、単に書き込めるだけの cgroup（`systemd --user` の slice など）は使わない。ほかの manager が管理しているためである。明示的に使わせたい cgroup は、library の `Cgroups::at(dir)` で指定する（委譲の判断は呼び出し側の責任）。

  どちらの場合も、`cgroup.kill`（Linux 5.14 以降）があることを、自分を移す前に確かめる。`memory` と `pids` の controller を有効にし（`cpu` は別に試し、有効にできなければ `cpu.max` を使わない）、上限を持つ試験用の cgroup を作って消せることを確かめてから使う。途中で失敗した場合は、有効にした controller と自分の移動を元に戻す。
- texrun が強制終了された（`SIGKILL` など）場合、その run の cgroup が残る。中のプロセスが終わった後も、空のディレクトリは残る。`Cgroups::detect` は、親の直下にある `texrun-<pid>.*` のうち、その pid のプロセスがもう無いものを `rmdir` する。`rmdir` は空の cgroup にしか成功しないので、プロセスのいる cgroup は消さない（kill もしない）。
- run（latexmk 1 回、preview tool 1 回）ごとに子 cgroup `texrun-<pid>.<n>` を作って上限を書く。supervisor は spawn の直後、子が gate で合図を待っている間に、子をこの cgroup に移す（exec gate でも rc の stdin gate でも同じ）。そのため、全ての子孫が最初から cgroup の中にいる。kill のたびに `cgroup.kill` にも書き（§3.6）、reap の後に `memory.events` の `oom_kill` と `pids.events` の `max` を読んでから cgroup を消す。
- gate を使わない preview（library で `StartMode::Immediate`）では、移動より前に起動された子孫が cgroup から漏れる（§3.6）。CLI では起きない。
- **使えない場合の扱い（fail-open / fail-closed）は CLI の `--cgroup` で選ぶ。**
  - `auto`（既定、fail-open）: 使えれば使う。使えなければ rlimit だけで compile し、結果の `resource_limits` に記録する（`"cgroup": false`、`notes` に理由）。
  - `required`（fail-closed）: 使えなければ、project をコピーする前に exit 3（`error.stage = "setup"`、`kind = "unsupported"`）で終わる。途中で使えなくなった場合（run の cgroup を作れない、移せない）も、engine は latexmk を動かさずに `EngineError::Unsupported` / I/O error を返す。preview は tool を動かさずに skip し、`resource_limits` の notice を出す。
  - `off`: 使わない。
  - 既定を fail-open にした理由: 委譲された cgroup は、ほとんどの環境に無い（端末の session の scope は root の所有、Docker の既定では cgroup の mount が read-only、GitHub Actions の runner のユーザーにも委譲されていない、macOS には無い）。fail-closed を既定にすると、texrun がほとんどの環境で動かなくなる。rlimit の層は、CLI では常に掛かる（gate は必須、§3.2）。
- library として使う場合は、`LatexmkConfig::with_cgroups` / `Previewer::with_cgroups` に `Cgroups::detect()`（または `Cgroups::at(dir)`）を渡す。`Cgroups::with_required(true)` が `--cgroup required` に当たる。
- 確認した環境: dev コンテナ（`docker compose run`、OrbStack）は `/sys/fs/cgroup` が read-only の mount で、cgroup は使えない（`auto` で rlimit だけになる）。`--privileged` のコンテナで `docker/dev/with-cgroup.sh` を使うと、namespace の root が使える。GitHub Actions の `test (linux)`（runner 上で直接実行）では使えない。cgroup の test はこれらの環境では skip し、CI の `integration` job で privileged のコンテナを使って実行する（[development.md](development.md#cgroup-test)）。

**上限に達したときの表現。** §3.2 の出力上限と揃える。

- `CompileOutcome::Failed` にし、`severity = error`、`kind = resource_limit` の diagnostic を付ける。message は「resource limit exceeded: ...」で、どの上限かを書く。専用の outcome は追加しない。消費側は、失敗の理由を diagnostic の kind で分けられるためである。
- timeout や cancel で止まった場合は、`TimedOut` / `Cancelled` のままにする。その前に上限に達していた場合は、同じ diagnostic も付ける。たとえば perl は `pids.max` で拒否された `fork` を 5 秒ごとに再試行するので、プロセス数の上限は timeout で終わることが多い。
- **成功した compile は `Failed` にしない。** プロセス数の上限は、一時的に達しても、perl の再試行で compile が最後まで進み、PDF ができることがある。その場合は `Succeeded` のままにし、`resource_limit` の diagnostic を warning で付ける。memory（`memory.oom.group` で全体が止まる）と CPU 時間（latexmk が 128 + signal で終わる）は、成功した compile では起きない。
- 判定の方法:

  | 上限 | 判定 |
  | --- | --- |
  | CPU 時間 | latexmk、または latexmk が起動したツールが `SIGXCPU` で終わった。texrun 管理 rc の `texrun_run` は、ツールが `SIGXCPU` / `SIGXFSZ` で終わったら latexmk を終了コード 128 + signal ですぐに終わらせる。latexmk 自身の終了コードは 128 未満で、文書から latexmk の終了コードは決められない |
  | memory（cgroup） | `memory.events` の `oom_kill` が 1 以上 |
  | address space（`RLIMIT_AS`） | 失敗した compile で、確保に失敗したプログラム自身のメッセージが、stderr に**行全体として**ある: kpathsea の `fatal: memory exhausted (xmalloc of <n> bytes).`、perl の `Out of memory!`（またはその行頭の形）。さらに、同じ行が main の `.log` に無いこと。これらのメッセージは stderr にだけ出る一方、文書が latexmk や TeX に出力させる文字列（ラベル名など）は行頭に来ないか（latexmk は `Latexmk:` の後やインデントの後に出す）、log にも残る。そのため、文書からこの判定は成立させられない |
  | プロセス数（cgroup） | `pids.events` の `max` が 1 以上 |
  | 1 ファイルのサイズ | §3.2（`SIGXFSZ` と、終了後の集計） |

- CLI: exit code は 1（compile の失敗）。JSON には diagnostic と、どの層が効いていたかの `resource_limits`（`{"rlimits": true, "cgroup": false, "notes": [...]}`）が入る。人間向けの出力は、diagnostic を error として表示し、最後の行を「Failed to compile ... : a resource limit was reached」にする。
- preview tool が上限に達した場合は、`limit_exceeded` の notice（warning）を出して preview をそこで止める。compile の結果と exit code は変わらない（§3.2）。

**テスト。** 上限を超える fixture は、防御の確認に必要な最小限にとどめる。

- `crates/texrun-process/tests/limits.rs`: CPU を使い続ける `sh` のループが `SIGXCPU` で止まる、`cpu.max` で throttle される（`nr_throttled`）、perl の大きな確保が `RLIMIT_AS`（Linux）と cgroup の `memory.max` で止まる、`sleep` を 40 個起動する script が `pids.max` で止まり、起動されたプロセスが残らない、新しい session に移ったプロセスも `cgroup.kill` で止まる（対照: cgroup が無いと残る）、run の cgroup が消える、soft と hard が別々に渡る、core を無効にしない CPU / file size の limit を拒否する。
- `crates/texrun-texlive/tests/limits.rs`: 既存の `timeout` fixture（無限ループ）が CPU 時間 2 s で `resource_limit` になる（macOS の library 利用で gate が無い場合は timeout になる）、`minimal` が小さい `RLIMIT_AS` / `memory.max` / `pids.max` で止まる、200 ページ超の文書（test の中で生成）が既定の上限に当たらない。
- `apps/texrun/tests`: CLI の JSON の `resource_limits`、`--cgroup auto / required / off`、委譲の印のある cgroup では自分を leaf に移し、印の無い cgroup には、root で書き込める場合も、texrun のユーザーの所有の場合も、何もしない（controller も有効にしない）こと、事前の確認の後に gate が消えた場合の `resource_limits` の notice。
- unit test: 委譲の印の判定（書き込めることや所有者では判定しない）、`nsdelegate` の読み取り、`RLIMIT_AS` のメッセージの行全体での照合（行の一部や log にもある行では成立しない）、一時的なプロセス数の上限で成功した compile が warning になること、engine の gate の分岐（exec gate、Linux の stdin gate、macOS の必須の gate）。

## 4. sandbox backend（#26）

TeX engine を texrun 本体と別の isolation boundary で動かす backend として、container backend を実装した（`--backend container`、library では `texrun_texlive::ContainerEngine`）。in-process 実行（`--backend host`）は local 開発用に残し、CLI の既定のままにする。

### 候補の比較と判断

| 候補 | 得られるもの | 主な課題 | 判断 |
| --- | --- | --- | --- |
| Docker | filesystem・network・PID・IPC の namespace、cgroup による CPU / memory / pids 制限、read-only の root filesystem と image の texmf | daemon が必要。macOS では VM を経由する | **最初の backend にする。** CI（GitHub Actions の runner）と開発環境（OrbStack、Docker Desktop）にあり、毎回 test できる |
| Podman（rootless） | Docker と同じ。daemon 無しで、container の root も host の非特権ユーザーになる | 環境による差（cgroup の委譲、`podman machine`）がある | **同じ CLI 互換の実装で受ける**（`--container-runtime podman`）。CI では test していない（#47） |
| gVisor（`runsc`） | container の隔離に加え、syscall を user-space kernel で仲介し、kernel の攻撃面を縮小する | Linux のみ、I/O 性能 | library の `ContainerConfig::oci_runtime`（`--runtime runsc`）で指定できるようにしたが、test していない。CLI には出さない |
| microVM（Firecracker 等） | VM 境界による強い隔離 | image と起動の管理が複雑で、KVM が必須 | 比較に留める |

- **runtime は CLI で呼ぶ。** `docker` / `podman` の実行ファイルを PATH（絶対パスの entry だけ）から探し、shell を使わず argv の配列で起動する。runtime の CLI は、daemon に接続するための変数（`DOCKER_HOST`、`XDG_RUNTIME_DIR` など、`texrun_sandbox::RUNTIME_ENV`）だけを持つ allowlist の環境で動く。engine に渡す環境とは別である。
- **runtime の検出と version の確認。** `--container-runtime auto`（既定）は Docker、次に Podman の順に、インストールされていて応答するものを使う。Docker は daemon の version（`Server.Version`）、Podman は client の version を読み、Docker 20.10 / Podman 4.0 未満と、Linux 以外の container を動かす daemon は使わない。
- **runtime は local のものに限る。** bind mount の source は daemon 側の host の path として解釈されるので、Docker は endpoint（現在の context と `DOCKER_HOST`）が `unix://` の socket の場合だけ使い、`tcp://` / `ssh://` などは `Unavailable` にする（Docker Desktop・OrbStack の socket は local）。Podman は `podman info` が応答することも必須にする（`podman machine` が止まっている場合を probe で検出する）。Podman の remote 接続（`podman machine` の既定）の場合、共有されている host の path だけが mount できる。runtime か image が無い場合は `EngineError::Unavailable`（CLI は exit 3、`error.stage = "probe"`）で、project をコピーする前に終わる。
- **image は texrun が pull しない**（`--pull never`）。compile 中に network へ出ないこと、使う image を利用者が明示的に用意することのためである。

### engine image

`docker/engine/Dockerfile`。dev image（`docker/dev`）とは別の、最小の runtime image である。

- base は `debian:trixie-slim` を multi-arch index の digest で固定し、Dependabot が digest を更新する。
- TeX Live のパッケージ（`latexmk`、`texlive-latex-base`、`texlive-latex-recommended`）は dev image と揃え、#10 の fixture が両方の backend で同じ結果になるようにする。preview tool（`mupdf-tools` / `poppler-utils`）も dev image と揃えて入れる（#46。AGPL / GPL の配布条件は README）。Rust toolchain やコンパイラは入れない。
- 既定の user は uid 10001 の非 root user で、`ENV` は `PATH` だけである。texrun は常に自分の `--user` を渡す。
- texrun が image に期待するもの: `/usr/bin/latexmk`、`/usr/bin` の pdflatex / bibtex / makeindex、`/usr/bin/timeout` と `/usr/bin/sleep`（coreutils）、`/usr/bin/prlimit`（util-linux）、`/usr/bin/mutool` または `/usr/bin/pdfinfo` + `/usr/bin/pdftoppm`（preview）。`/workspace` と `/texrun` には何も置かない。
- CI の `sandbox` job が毎回 build する（layer は GitHub Actions の cache に置く）。既定の image は、その版の texrun のために公開した `ghcr.io/nishide-dev/texrun-engine:<version>`（`texrun_sandbox::DEFAULT_IMAGE`、`--container-image` で変えられる）。公開の仕組みは次の「engine image の公開」を参照。

### engine image の公開（#48）

`.github/workflows/engine-image.yml` が、release（`v*` の tag の push）ごとに engine image を GHCR に公開する。

- **tag と対応。** `ghcr.io/nishide-dev/texrun-engine:<version>`（`linux/amd64`、`linux/arm64`）。`<version>` は tag から `v` を除いたもので、`Cargo.toml` の `workspace.package.version` と一致しない場合と、tag の commit が main の祖先でない場合は公開しない。image の label `org.opencontainers.image.version` にも同じ版を入れる。
- **上書きしない。** registry が「その tag は無い」（`not found` / `manifest unknown` / `name unknown`）と答えた場合だけ続け、既にある場合とそれ以外のエラー（network、認証、rate limit）では公開を止める（fail-closed）。確認は push の前と tag を付ける直前の 2 回行い、同じ tag の実行は concurrency group で直列にする（tag の push と、同じ tag での手動の公開が同時に走らない）。利用者が digest で固定した image と、その版の tag が指す image が常に一致する。`latest` などの動く tag は付けない。
- **test した image を公開する。** `verify` が platform ごとの native の runner（amd64 は `ubuntu-latest`、arm64 は `ubuntu-24.04-arm`）で image を build し、CI の `sandbox` job と同じ container backend の test と、`engine.version` に `image version <version>` が出ることを確かめ、image の layer（`RootFS` の diff ID）を記録する。`publish` はその layer（GitHub Actions の cache）を使って multi-arch で build し、**tag を付けずに digest で push** する。そのうえで、platform ごとに push した image の diff ID が `verify` の記録と一致することを確かめてから、attestation を作り（digest に付くので tag の前に作る。attestation が失敗しても、版の tag はまだ無いのでやり直せる）、最後に tag を付ける。cache が消えていた場合などで apt が別の版を入れると一致しないので、tag は付かない（digest だけの image が残る。GHCR から消してよい）。arm64 は、`verify` が native の runner で build した layer の cache に、`publish` の QEMU での build が hit することを前提にしている。これは実際の公開まで確かめられないが、hit しなければ layer の比較で止まる（安全側）。
- **権限。** `verify` と `sources` は書き込みの権限を持たない（`verify` の test は workspace の crate を build・実行するため）。`packages: write`（push）と `id-token: write` / `attestations: write`（署名付きの attestation）は `publish` だけ、`contents: write`（release の作成と asset の upload）は `release` だけが持つ。
- **source の同梱。** `sources` が、`verify` が両方の platform の image で `dpkg-query` した source package と版の一覧から、Debian の source package（`.dsc`、`.orig.tar.*`、`.debian.tar.*`）を、その版ちょうどで取得する（`.github/scripts/fetch-engine-sources.sh`）。archive から取る場合は、apt が署名付きの Release / Sources で検証する。archive に無い版は snapshot.debian.org から取り、各ファイルを SHA-1（snapshot での address）で、package 全体を `.dsc` の checksum で照合する。ただし `.dsc` 自体の署名は検証しない（過去の全 uploader の鍵が要り、keyring からは期限切れの鍵が消えるため）。そのため、snapshot から取った package の真正性は snapshot.debian.org への HTTPS に依存する。1 つでも取得・照合できなければ失敗し、`publish` は `sources` の成功を待つので、source を添えられない版は公開されない。`release` が、それを tag の GitHub release に `texrun-engine-<version>-sources.tar` として付ける（release が無ければ作る）。GPLv2 §3(a)・GPLv3 / AGPLv3 §6(a)（binary に source を添えて配布する）に当たる。GPL / AGPL のものに限らず、image の全パッケージの source を付ける。
- **SBOM と provenance。** buildx の `--sbom`（dpkg の database から作る SPDX。image の全パッケージと版）と `--provenance=mode=max`（SLSA provenance）を image と一緒に registry に置く。加えて `actions/attest` で、Sigstore で署名した GitHub の artifact attestation を作る（`gh attestation verify oci://ghcr.io/nishide-dev/texrun-engine:<version> -R nishide-dev/texrun`）。image の中の apt パッケージの版は build の日で決まるので、公開した image の SBOM が「その版の image に何が入っているか」の記録になる。
- **PR と手動実行。** `docker/engine/` か workflow（とその script）が変わる PR と、`workflow_dispatch`（既定は `publish` が off）では、`publish` と `release` 以外を実行する。`verify`（両 platform の test）、push しない multi-arch の build（両方の platform の image と SBOM / provenance ができること）、`sources`（source の取得と検査。保存はしない）である。`publish` を on にした手動実行は、`v*` の tag の上でだけ公開する（失敗した公開のやり直し用）。
- **release の入口。** `v*` の tag は write 権限があれば作れるので、repository の設定で、`v*` の tag の ruleset（作成・更新・削除を owner に限る）と、`publish` / `release` の job に `environment`（必須の reviewer）を付けることを勧める（workflow は main の祖先の確認だけを行う）。
- **既定の image の参照に digest を使わない理由。** texrun の binary に image の digest を入れるには、image を build してから texrun を build する 2 段の release が要る（同じ tag から作る image の digest は、その tag の source には書けない）。そこで既定は版の tag にし、次のことで対応を保つ。
  - 版の tag は上書きしない（上記）。
  - texrun は pull しない（`--pull never`）ので、tag が指す image は利用者が明示的に取得したものだけである。
  - `engine.version` に image の ID と版の label を出し、texrun の版と違えば `image version X, not Y of texrun` と書く。
  - 固定したい利用者は `--container-image ghcr.io/nishide-dev/texrun-engine@sha256:<digest>` を使える（compile は、probe で解決した image の ID で行う）。
- **ローカルの build。** `docker build -t ghcr.io/nishide-dev/texrun-engine:<version> docker/engine`、または任意の名前と `--container-image`。label が無いので `engine.version` は `image without a version label` になる。開発用の test は、既定で `texrun-engine:latest`（`TEXRUN_SANDBOX_IMAGE` で変えられる）を使う。
- **配布条件。** image は texrun の code を含まず、Debian の未改変のパッケージ（TeX Live、latexmk、coreutils、util-linux、preview 用の MuPDF（AGPL）と Poppler（GPL）など）から成る。各パッケージの license は image の `/usr/share/doc/<package>/copyright` に残し、対応する source は同じ tag の GitHub release に付ける（上記、README の「Engine image」）。

### container の設定

latexmk 1 回の compile ごとに container を 1 つ作る（`texrun_sandbox::Container`）。

| 設定 | 値 | 目的 |
| --- | --- | --- |
| network | `--network none` | network 無し（#24） |
| root filesystem | `--read-only`、`/tmp` だけ `tmpfs`（`noexec,nosuid,nodev`、64 MiB） | image（texmf を含む）を書き換えさせない |
| mount | workspace を `/workspace` に read-only、output dir と `HOME`（`.texrun/home`）をその位置に書き込み可、rc のディレクトリを `/texrun/rc` に read-only。ほかは何も mount しない | host の filesystem を workspace 以外見せない。入力を書き換えさせない |
| user | texrun の euid / egid（texrun が root なら image の `texrun` user の 10001。host の既存の user（`nobody` など）と共有しない。書き込み可の mount はその uid に渡す）。root では動かさない。rootless Podman では `--userns keep-id` も付ける | 非 root。書いたファイルは texrun のユーザーのものになる |
| 権限 | `--cap-drop ALL`、`--security-opt no-new-privileges`、`--ipc none` | capability と権限の獲得を無くす |
| 上限 | `--memory` = `--memory-swap`（4 GiB、swap 無し）、`--pids-limit 64`、`--cpus 2`、`--ulimit`（CPU 時間・ファイルサイズ・core）、container 内の `prlimit`（address space） | §3.10 の既定値と揃える |
| 環境変数 | §3.4 の allowlist を `--env` で渡す（`PATH` は image の `/usr/bin:/bin`、`HOME` は container 内の path）。runtime が付ける `HOSTNAME`（`texrun`）以外は無い | host の環境変数を渡さない |
| そのほか | `--init`（PID 1 が孤児を回収し、signal を中継する）、`--log-driver none`（出力は texrun に流すだけで、daemon に残さない）、`--pull never`、`--hostname texrun` | |

- **texmf** は image の中の TeX Live で、root filesystem ごと read-only である。host の texmf は mount しない。
- **§3 の設定はそのまま使う。** texrun 管理 rc（§3.5）、latexmk の引数、env allowlist（§3.4）、kpathsea 設定（§3.7）、timeout と出力の上限（§3.1・3.2）、log と BibTeX の diagnostics は、in-process 実行と同じ code（`texrun-texlive` の内部）で作る。変わるのは、latexmk に渡す path（rc、`-outdir`、cwd、`HOME`）が container の中のものになることと、上限を掛ける主体が runtime になることだけである。rc は container 内では stdin gate を持たない（上限は latexmk の起動前に runtime が掛けるため）。
- 出力サイズの監視（§3.2 の poll）は、host 側から mount 元のディレクトリに対して行う。

### container のライフサイクル

- texrun は container を `create`（名前 `texrun-<pid>-<n>-<nanos>` と label `org.texrun.sandbox=1`、`org.texrun.sandbox.pid=<pid>`）で作り、`start --attach` を supervisor（§3.6）の子プロセスとして起動する。stdout / stderr と終了コードは container のもので、latexmk の終了（`128 + signal` を含む、§3.10）はそのまま判定に使える。
- **作った container の制限を確かめる（fail-closed）。** daemon は、kernel や cgroup の構成が対応していない上限（`--memory` など）を、stderr に警告を出すだけで捨てて `create` に成功する。そのため texrun は `create` の後に `inspect` で `HostConfig` を読み、memory / memory+swap / pids / CPU、`--ulimit`、network mode、read-only の root、capability、`no-new-privileges`、privileged でないこと、seccomp などの profile を無効にしていないことを、要求と照合する。一つでも違えば container を起動せずに消し、`EngineError::Unavailable`（CLI は exit 3）にする。`create` の stderr（警告）は、結果の `resource_limits.notes` に `container: ...` として残す。
- `create` 自体が失敗・timeout した場合も、daemon 側で作られている可能性があるので、texrun が付けた名前で `rm --force` を 1 回試みる。
- compile は、probe で確かめた image の ID（`sha256:...`）で `create` する。probe と compile の間に tag が付け替えられても、`engine.version` で報告した image が使われる。
- runtime の CLI を kill しても container は止まらない。そのため supervisor の kill のたび（timeout、cancel、出力上限、正常終了の後の念のための kill）に、container の状態（OOM kill、終了コード）を読んでから `rm --force` で消す（中で動いているものも kill される）。消せなかった場合は、reap の後と texrun 側の値の drop で再試行する。`create` した container は、`start` しなかった場合も含めて、texrun の全ての経路で消える。
- texrun 自身が `SIGKILL` などで終了した場合は、container（と `start --attach` の runtime の CLI）が残る。その場合も、container の中の `timeout --signal=KILL` が compile の timeout + 45 秒（CPU 時間の余裕 15 秒 + 30 秒）で engine を止め（中の CPU 時間の上限が先に効くこともある）、container は停止した状態で残る。残った container は label で見つけて消せる（`docker ps -a --filter label=org.texrun.sandbox`）。次回の起動時に自動で回収することは #49 で扱う。
- 実測（OrbStack、macOS、arm64）: 1 ページの文書で CLI 全体が約 1.4 秒（probe の `latexmk -v` の container、compile の container の作成・削除を含む）、そのうち container の中の latexmk の実行は約 0.2 秒だった。

### path mapping と diagnostics

- `CompileRequest` と `CompileResult` は `WorkspacePath`（workspace root 相対）だけでファイルを参照するので、backend によらず同じである。
- container の中で workspace が見える場所は、`CompileContext::path_mapping`（`texrun_core::PathMapping`）で表す。`ContainerEngine` は、context に mapping が無ければ `/workspace` を使い、あればその path に mount する。mount 先は `/workspace`（とその下）か、`/srv` / `/mnt` の下だけを許し（`texrun_texlive::GUEST_ROOT_PARENTS`）、それ以外は `InvalidRequest` にする。`/usr` や `/etc` などに mount すると、信頼できない workspace が image の実行ファイルや設定（latexmk、`timeout`、`prlimit`、pdflatex）を覆い隠し、§3 の層が container の中で意味を持たなくなるためである（`/texrun` の rc、runtime の mount する `/proc` / `/dev` / `/tmp` などとの衝突も防ぐ）。host で latexmk を動かす `LatexmkEngine` は、mapping のある context を `Unsupported` で拒否する。
- engine は、latexmk に渡す path だけを mapping で container の path にし、workspace の読み書き（出力の収集、log と source の読み込み）は host の path で行う。log の parser には、TeX が見ていた working directory（container の path）を root として渡すので、log の中の絶対パスも workspace 相対の `Diagnostic::file` になる。mount 先を変えても diagnostics が同じになることを test で確認している。

### preview

- `--backend host` の preview（#8）は host で動かす（exec gate、rlimit、`--cgroup`、§3.2・3.10）。
- `--backend container` の preview は、engine image の preview tool を **container の中で** 動かす（#46、library では `Previewer::in_container(PreviewContainer)`）。host の preview tool は使わない（インストールされていなくてよい）。container が使えない場合も host には fallback せず、preview を skip して notice を出す。
- **container は preview 1 回に 1 つ**（`texrun_sandbox::Session`）。中で `sleep` を main process として動かし、tool（`mutool` / `pdfinfo` / `pdftoppm`）は `exec` で 1 回ずつ順に起動する。preview は 1 回で最大 200 ページ描画するので、起動のオーバーヘッドで決めた。実測（OrbStack、macOS、arm64、`mutool draw` 144 dpi）:

  | 方式 | 1 回あたり |
  | --- | --- |
  | ページごとに container（`create`・`inspect`・`start --attach`・`rm`） | 約 0.23 秒 |
  | container 1 つ（`create`・`inspect`・`start`、最初の 1 回だけ） | 約 0.21 秒 |
  | その中で tool を `exec`（描画を含む） | 約 0.035 秒 |

  ページごとの container では、200 ページで起動だけで約 46 秒掛かり、preview の timeout（30 秒）を超える。container 1 つの方式では、CLI の `--pages 1-200` で 200 ページの preview が MuPDF で約 12 秒、Poppler で約 16 秒（compile を除く。既定の 20 ページは MuPDF で約 1.6 秒）だった。Poppler では preview の timeout（30 秒）に対する余裕が 2 倍弱なので、遅い host では上限（200 ページ）に近いページ数で timeout しうる（その場合も描画済みのページは残り、`timed_out` の notice が付く）。
- **container の設定** は compile の container と同じ（`--network none`、`--read-only`、`--cap-drop ALL`、`no-new-privileges`、`--ipc none`、非 root、`--init`、`--pull never`、label）で、`create` の後の `HostConfig` の照合も同じく行う（照合が通らなければ起動せずに消し、preview は `resource_limits` の notice で skip、fail-closed）。上限は §3.10 の preview tool の値である:
  - cgroup: `--memory` = `--memory-swap` 2 GiB、`--pids-limit 32`、`--cpus 2`（tool は順に動くので、実質 tool 1 回ごとの上限）
  - rlimit: tool ごとに `prlimit` で `RLIMIT_AS` 2 GiB、`RLIMIT_FSIZE`（残りの画像の予算、最低 16 MiB）、`RLIMIT_CPU`（preview の timeout + 10 秒、hard はその 5 秒後）、`RLIMIT_CORE` 0 を掛けてから tool を起動する。container 全体の `--ulimit` はこれらの最大値で、`prlimit` はそれより上げられない
  - CPU 時間・memory の上限に達した tool は、終了コード（`128 + SIGXCPU`）と runtime の `OOMKilled` で `limit_exceeded` にする
- **mount** は、texrun が preview ごとに作る scratch directory（既定は system の temp dir の下。output root の外）の 3 つだけである。`in`（PDF のコピー、read-only、`/texrun/preview/in`）、`work`（tool の作業ディレクトリ、書き込み可）、`home`（tool の `HOME`、書き込み可）。runtime は mount 元を path で解決するので、compile の残りのプロセスが書き込みえた output root の中には置かない。scratch directory は `mkdirat` で作って descriptor で保持し、PDF は descriptor 経由でコピーする。
- **画像の保存は descriptor で行い、コピーを検査する**（§3.2、§2「sandbox backend でも保証しないもの」）。tool は `work` に書く。`work` は container から書き込めるので、texrun はそこで検査も rename もしない。保持した `work` の descriptor から画像を `O_NOFOLLOW` で開き（通常ファイルに限る）、preview ディレクトリ（`openat(O_NOFOLLOW)` で開いたもの。container からは見えない）に `O_EXCL` で作った texrun 専用のファイルへ、残りのサイズ予算 + 1 byte を上限にコピーする。サイズ・PNG の header・画素数はコピーの descriptor で確かめ、通ったものだけを `renameat` で `page-NNN.png` にする。保存した画像は container 側と inode を共有しない（filesystem が同じでも別のファイル）。symlink はたどらない。描画先のファイル名はページごとに変える（macOS の runtime の file sharing が、host で移動した名前を container 側でしばらく保持するため）。
- **停止。** timeout・cancel・出力サイズの上限で tool の `exec` を kill した場合、runtime の CLI を kill しても container の中の tool は止まらないので、container ごと `rm --force` で消す（その preview は止まる）。preview の終了時（正常終了を含む）にも container を消し、その後に scratch directory を消す。texrun が強制終了された場合は、`sleep` が preview の timeout + 45 秒で終わり、停止した container と scratch directory（`texrun-preview-*`）が残る（label で見つけて消す、#49）。

### CLI

- `--backend host|container`。既定は `host` のままにする。container backend には container runtime と engine image が必要で、既定にすると、それが無いほとんどの環境で texrun が動かなくなるためである。信頼できない文書には `--backend container` を使う（README に記載）。
- `--container-runtime auto|docker|podman`、`--container-image <IMAGE>`。`--backend host` と一緒に指定すると usage error（exit 2）にする。
- `--cgroup` は host で動くプロセス（`--backend host` の engine と preview tool）に効く。container backend の engine と preview tool には、container の cgroup が常に掛かる。

### 今後（xelatex / lualatex など）

将来 xelatex / lualatex / dvipdfmx を追加するときは、それぞれが起動する外部プログラムも §3.5 と同じ基準で扱う。

- xelatex は出力 driver（`xdvipdfmx`）を子プロセスとして起動する。
- dvipdfmx は画像変換に外部プログラム（Ghostscript 等）を使う設定を持つ。
- LuaTeX は Lua からのプロセス起動・ファイルアクセスを持つ。

それぞれについて、次のことを確認する。

- shell を経由せず起動されるか
- 変換コマンドの設定を固定できるか
- restricted / safer 系の option（例: LuaTeX の `--safer` 相当）が使えるか

必要なら rc と env allowlist を拡張する。sandbox backend では、これらのプログラムも image に入れ、同じ container の制限の中で動かす。

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
| 大きな文書・画像の CPU 時間と memory（#25） | §3.10 の表。417 ページの文書の latexmk 全体で memory.peak 47 MiB、pids.peak 10 |
| `RLIMIT_CPU` の soft = hard（Linux） | `SIGXCPU` が来ずに `SIGKILL` で終わった。soft < hard では soft で `SIGXCPU` になった（§3.10） |
| `pids.max` を 1 にした cgroup での latexmk | perl の `system` が `fork` の失敗を再試行し続け、timeout で止まった |
| cgroup の mount | `docker compose run` のコンテナでは read-only（`0::/`）。`--privileged` では書き込めた |
| 生成物の host 情報 | `.fls` に workspace の絶対パスが入った。PDF には作成日時と pdfTeX / TeX Live の banner が入った。`SOURCE_DATE_EPOCH=0` と `FORCE_SOURCE_DATE=1` で、日時が固定された |
