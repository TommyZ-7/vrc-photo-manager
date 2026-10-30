# 圧縮メモリピーク削減と写真単位デコード共有 設計

- Issue: #192, #194
- 日付: 2026-10-30
- 状態: 承認済み

## 目的

圧縮時のメモリピークと、複数プロファイル圧縮の総時間を同時に削減する。

1. `crates/core/src/compressor.rs` のエンコーダが原画のフルサイズコピーを
   複数保持している。特に AVIF は `to_rgba8()` に加えて
   `Vec<rgb::RGBA8>` を collect しており、4K/8K で数百MB単位の無駄になる。
2. デーモンはプロファイル単位のジョブを1件ずつ取得し、同じ写真でも
   プロファイルごとに原画をフルデコードしている。プロファイル2件なら
   デコードも2回になる。

両者は「デコード済み画像を所有から借用へ変える」点で共通するため、一括で対応する。

## ゴール

- AVIF / WebP / JPEG / JPEG XL のエンコード経路から、不要なフルサイズコピーを除く。
  - AVIF は **RGBA 経路を維持**し、透過（アルファ）を保持する。
    RGBA 入力はゼロコピー、RGB 入力は RGBA への1回変換のみ。
- `compress_decoded` を `&DynamicImage` 受け取りに変え、共有デコードと両立させる。
- デーモンは「最も古い pending ジョブの写真」について pending ジョブを
  まとめて原子的に取得し、同一バッチ内では1回だけデコードする。
  バッチ取得後に追加されたジョブは別バッチとなり、再度デコードし得る
  （連続処理の範囲に限定する。長時間キャッシュはしない）。
- バッチ内の各ジョブは従来どおり個別に成功・失敗し、1件の失敗が
  同じ写真の他ジョブを失敗させない（デコード失敗を除く）。
- `replace` の入力パス規則を変えない。バッチ開始時点の元画像パスを固定し、
  先行ジョブが `replace` を完了しても後続ジョブは派生ファイルを読まない。
- 強制削除（`force_deletion`）の既存契約を保つ。実行中のジョブは完了させ、
  バッチ内の残りジョブは `cancelled` にして削除を待たせない。
- 4K / 8K のピークメモリと、プロファイル2件の総時間を実測して記録する。

## 非ゴール

- アロケータの差し替え（#193）。
- 圧縮エンコーダの品質・速度・出力形式の変更。
- AVIF の RGB 経路（`ravif::Encoder::encode_rgb`）への変更。
  透過画像を扱うため、常に RGBA 経路を通す。
- DBスキーマ変更、IPCプロトコル変更。
- デコード済み画像をバッチ外へ保持するキャッシュ。

## 現状

- `compress_image` が `decode_image` して `compress_decoded` に所有権で渡す。
- `resize_for_profile` は `DynamicImage` を消費する。
- 各エンコーダ:
  - AVIF: `to_rgba8()` → `Vec<rgb::RGBA8>` に collect → `encode_rgba`。
  - WebP: `to_rgba8()` → `Encoder::from_rgba`。
  - JPEG: `to_rgb8()` → `encode(..., Rgb8)`。
  - JXL: `to_rgb8()` → `encode`。
- デーモン:
  - `worker_loop` と `compress_pending` が `Db::next_pending_job()` で1件取得。
  - `process_job` が写真・プロファイルを読み、`compress_image` で毎回デコード。
  - ワーカーは最大4本。同一写真のジョブが別ワーカーに取られると、
    デコードとエンコーダ作業領域が同時に重複する。

## 検討した方式

### デコード共有

#### A. 写真単位バッチ取得（採用）

最も古い pending ジョブの `photo_id` を選び、その写真の pending ジョブを
1つの `UPDATE ... RETURNING` でまとめて `processing` にする。
ワーカーはバッチ開始時に1回だけデコードし、プロファイルごとに順次エンコードする。

- 利点: 写真ごとのデコードが構造的に1回になる。同一写真のエンコードが
  直列になるため、現状で起こり得る「同一写真を複数ワーカーが同時に
  デコード・エンコードする」ピークも下がる。実装が決定的でテストしやすい。
- 欠点: 同一写真のプロファイルエンコードは並列化されない。
  ただしデコード削減分は確実に効き、他写真は別ワーカーが並行処理できる。

#### B. 写真IDごとのデコードキャッシュ（不採用）

`Arc<DynamicImage>` を写真IDで共有し、ジョブ単位の並列処理を維持する。

- 利点: 同一写真のエンコードも並列化できる。
- 欠点: キャッシュの寿命・参照カウント・破棄の管理が必要。
  並列エンコードによりエンコーダの作業領域が同時に増え、
  #192 の「ピークを増やさない」制約と衝突する。

#### C. ワーカーごとの直近デコード保持（不採用）

各ワーカーが最後に処理した写真だけ保持する。

- 利点: 変更が小さい。
- 欠点: 同一写真のジョブが別ワーカーに取られると共有できない。
  ワーカー数やキュー順で効果が変わり、保証できない。

### エンコーダのコピー削減

#### A. 借用 + バイト再解釈（採用）

`DynamicImage` を借用し、既に RGBA8 / RGB8 の場合は `as_rgba8()` /
`as_rgb8()` の生バッファをそのまま使う。AVIF の `rgb::RGBA<u8>` は
`bytemuck` で再解釈し、`Vec` への collect をなくす。

- 利点: `compress_decoded(&DynamicImage)` と両立する。
  `into_rgba8` のような所有権消費が不要。RGBA 入力は完全ゼロコピー。
- 欠点: RGB 入力の AVIF/WebP は RGBA への1回変換が残る。
  これはエンコーダ API の要件上避けられない。

#### B. `into_rgba8` で所有権を移す（不採用）

- 利点: RGBA 入力の変換コピーを確実に避けられる。
- 欠点: 画像を消費するため、複数プロファイルで共有できない。
  #194 と両立しない。AVIF の `Vec<RGBA8>` collect も別途消す必要がある。

#### C. AVIF を `encode_rgb` に変える（不採用）

- 利点: RGB 入力の RGBA 変換をなくせる。
- 欠点: 透過画像のアルファが失われる。VRChat は透過PNGを撮影できるため不可。

## 方式Aの詳細

### core: 圧縮API (`crates/core/src/compressor.rs`)

- `pub fn compress_decoded(image: &DynamicImage, source: &Path, profile: &CompressionProfile)`
  に変更する。
- `resize_for_profile(image: &DynamicImage, profile) -> Cow<'_, DynamicImage>`:
  - リサイズ不要なら `Cow::Borrowed(image)`。
  - 必要なら従来どおり Lanczos3 で `DynamicImage` を作り `Cow::Owned`。
- `compress_image` はデコード後に `compress_decoded(&image, ...)` を呼ぶ。
- `render_clipboard_png` も新しい `resize_for_profile` に合わせる。

### core: エンコーダ

- `encode_avif`:
  - `image.as_rgba8()` があればその生バッファを借用する。
    なければ `image.to_rgba8()` を1回だけ作る。
  - `rgb::bytemuck::cast_slice::<u8, rgb::RGBA<u8>>` で
    `Img<&[rgb::RGBA<u8>]>` を作り、`encode_rgba` に渡す。
  - `Vec<rgb::RGBA8>` の collect を削除する。
  - 透過画像でも従来どおり `encode_rgba` を通す。
- `encode_webp`:
  - `webp::Encoder::from_image(image)` を使い、RGB8 / RGBA8 を直接渡す。
  - 非対応の色形式だけ `to_rgba8()` にフォールバックする。
- `encode_jpeg`:
  - RGB8 / RGBA8 は `JpegEncoder::encode_image` にそのまま渡す
    （RGBA はエンコーダ側でアルファを落とす）。
  - それ以外は従来どおり `to_rgb8()`。
- `encode_jxl`:
  - RGB8 は `as_rgb8()` の生バッファを直接渡す。
  - それ以外は従来どおり `to_rgb8()`（JXL はアルファを扱わない）。
- `Cargo.toml`: `rgb` の `bytemuck` feature を有効にする。
  `rgb::bytemuck` の再エクスポートを使うため、直接依存は追加しない。

### core: DB (`crates/core/src/db.rs`)

- `pub fn next_pending_photo_jobs(&self) -> Result<Vec<CompressionJob>>` を追加。
  - 最も古い pending ジョブの `photo_id` をサブクエリで選ぶ。
  - その写真の pending ジョブを `UPDATE ... RETURNING` で一括して
    `processing` にし、`started_at` を設定する。
  - 返却順は enqueue 順（`rowid` 昇順）。`rowid` は RETURNING の末尾列で受け、
    Rust 側でソートする。
  - pending がなければ空 `Vec`。
  - `processing` ジョブが既にある写真も対象にする。バッチ取得後に追加された
    ジョブは別バッチになる（1写真1バッチの強制ロックはしない。
    クラッシュで `processing` が残った場合に恒久ブロックするため）。
- `pub fn photo_has_forced_deletion(&self, photo_id: i64) -> Result<bool>` を追加。
  - `deletion_requests` に `force = 1` かつ `status IN ('pending', 'blocked')`
    の行があるかを返す。バッチ途中の残りジョブを `cancelled` にする判断に使う。
- 既存 `next_pending_job()` はテスト用に残す。デーモンの実行経路は
  バッチ版へ切り替える。

### daemon (`crates/daemon/src/lib.rs`)

- `worker_loop`:
  - `next_pending_photo_jobs()` でバッチ取得する。空なら従来どおり待機。
  - 取得直後に各ジョブへ `Processing` / 0.05 のイベントを出す。
  - 写真・プロファイル・入力パスをバッチで1回だけ解決する。
  - `decode_image` を1回だけ実行する。
  - 各ジョブを `compress_decoded(&image, ...)` で処理する。
  - ジョブとジョブの間で `photo_has_forced_deletion` を確認し、true なら
    残りのジョブを `cancelled` にしてバッチを打ち切る。
    処理中の1件は完了させる（既存の強制削除契約と同じ）。
- `compress_pending` も同じバッチ取得・処理ヘルパーを使う。
  併せて `compress_pending` から `start_workers` / `stop_workers` を外し、
  呼び出しスレッドだけがジョブを取得するようにする。現状はメインスレッドと
  圧縮ワーカーが同時に取得するため、計測と実行順が非決定的になる。
  削除は既存どおり最後の `process_deletions` で実行する。
- 入力パスの固定:
  - バッチ開始時に `photo.abs_path` がファイルとして存在すればそれを使い、
    なければ `photo.effective_path()` を使う（現行 `process_job` と同じ規則）。
  - そのパスをバッチ内の全ジョブで共有する。
  - 先行ジョブの `replace` で `display_derivative_id` が変わっても、
    後続ジョブは固定済みの元画像パスを読む。
- ジョブ単位の処理:
  - プロファイルを都度読み、`output_dir` のジョブ上書きを適用する。
  - `compress_decoded` の結果を検証し、derivative 登録、`replace` 処理、
    ジョブ完了更新を行う（現行 `process_job` の後半を共通化）。
- 失敗分離:
  - 写真不存在・デコード失敗などバッチ全体の準備失敗は、バッチ内の
    全ジョブを `failed` にする（従来も各ジョブが同じ理由で失敗していた）。
  - プロファイル不存在・エンコード失敗・出力検証失敗は、そのジョブだけ
    `failed` にし、残りを継続する。
  - イベントはジョブ単位で `Completed` / `Failed` を出す。
- 既存の `process_job(db, job)` はテスト用の単一ジョブ入口として残し、
  バッチ処理に委譲する。

### メモリのライフサイクル

- デコード済み画像は1バッチ（1写真）の間だけ保持し、バッチ終了時に破棄する。
- バッチ内でもエンコードはプロファイルごとに直列で、前のエンコーダの
  作業領域が解放されてから次へ進む。
- 複数ワーカーは別々の写真を処理するため、従来の同時実行数を超えない。
  同一写真のジョブが複数ワーカーに分散しなくなる分、ピークは下がる。

## テスト

### core

- AVIF:
  - 透過RGBA画像を圧縮・再デコードし、アルファが保持される
    （透明画素が透明のまま、不透明画素が不透明のまま）。
  - RGBA 入力と RGB 入力の両方が圧縮でき、出力が空でない。
  - 既存の `recompresses_an_avif_derivative` が通る。
- WebP / JPEG / JXL:
  - RGB8 / RGBA8 の高速経路が従来の変換経路と同じ画素結果になる
    （可能ならバイト一致、難しければ再デコード後の画素比較）。
  - 非8bit・特殊色形式のフォールバックが動く。
- `compress_decoded(&image, ...)` を同じ画像に対して2回呼び、
  両方の出力が生成され、元画像が消費されないことを確認する。
- リサイズあり／なしの既存テストが通る。

### core DB

- 同一写真に3件、別写真に1件の pending ジョブがあるとき、
  最初のバッチが同一写真の3件だけを enqueue 順で返す。
- `waiting` ジョブは取得しない。
- バッチ取得後、`next_pending_photo_jobs()` の次回呼び出しが別写真を返す。
- 既存の `next_pending_job()` のテストは維持する。

### daemon

- プロファイル2件・写真1枚の圧縮で、両ジョブが完了し derivative が2件できる。
- `replace` ジョブの後に `keep` ジョブを同一バッチで処理しても、
  `keep` の出力が派生ファイルではなく元画像由来になる
  （リサイズ設定を変えて寸法で検証）。
- 1ジョブだけ出力先エラーにしても、他ジョブは完了する。
- バッチ取得後、1件目の処理中に強制削除が入った場合、1件目は完了し、
  残りは `cancelled` になる（既存の強制削除テストの契約を維持）。
- `compress_pending` が圧縮ワーカーを起動せず、全 pending を処理する。
- 既存の削除待ちテスト（全ジョブ完了まで元画像を消さない）が通る。
- 既存の複数ワーカー・再試行テストが通る。

## 計測

### #192: ピークメモリ

- リリースビルドで、4K（3840×2160）と 8K（7680×4320）の
  決定的な RGB8 / RGBA8 PNG を用意する。
- 形式ごとに別プロセスで `compress_image` を1回実行し、
  `/usr/bin/time -v` の `Maximum resident set size` を記録する。
- 比較対象: 変更前の `main` と変更後のブランチ。同じ画像・同じ
  プロファイル（品質60、速度8、リサイズなし）を使う。
- 結果は PR と Issue #192 に記録する。計測手順と代表値は
  `docs/development.md` に残す（docs PR）。

### #194: プロファイル2件の総時間

- 同じ4K PNG に対し、WebP（品質80）と AVIF（品質60）の2プロファイルを
  1枚に適用する。
- 単一ワーカー相当の処理ループで、ジョブが全て終端するまでの壁時計時間を
  3回計測して中央値を比較する。変更前 `main` は `process_job` を1件ずつ、
  変更後ブランチはバッチ処理を使う（`compress_pending` は現状メインスレッドと
  ワーカーが競合するため計測に使わない）。
- 併せて `/usr/bin/time -v` の user+sys（総CPU時間）を記録し、
  並列度に依存しない「総仕事量」の削減も確認する。
- `compression_workers >= 2` では、変更前は同一写真のプロファイルが
  別ワーカーで並列になり得るため、壁時計時間は改善しないか悪化し得る。
  本変更はピークメモリと総仕事量を優先するトレードオフとして記録する。
- 比較対象は #192 と同じく変更前 `main` と変更後ブランチ。
- 結果は PR と Issue #194 に記録する。

## ドキュメント

- `docs/architecture.md` 4.4: 写真単位バッチ取得・1回デコード・
  入力パス固定を記載。
- `docs/features.md` 3.3: 同一写真の複数プロファイルはデコードを
  共有し、バッチ外に保持しないことを記載。
- `docs/development.md`: 圧縮のピークメモリ／時間計測手順と代表値を追加。
- `docs/testing.md`: core / daemon の追加テストを反映。

## リスクと対策

- **バッチ取得でキューが偏る**: 1写真あたりの pending はプロファイル数と
  手動再実行に限られ、通常は数件。他写真は別ワーカーが処理する。
- **`replace` の二重圧縮**: バッチ開始時に元画像パスを固定し、
  後続ジョブへ派生ファイルを渡さない。回帰テストで寸法を検証する。
- **強制削除が遅れる**: バッチ内の残りジョブをジョブ間で `cancelled` にし、
  既存の「処理中1件だけ完了させる」契約を維持する。
- **ワーカー2本以上での壁時計時間**: 同一写真のエンコードが直列になるため、
  変更前に比べて壁時計が伸びる場合がある。ピークメモリと総仕事量を優先し、
  計測結果にトレードオフとして明記する。
- **透過の喪失**: AVIF は常に `encode_rgba`。透過RGBAの回帰テストを追加する。
- **出力差**: AVIF は同じ RGBA 画素を渡すため従来と同一。WebP/JPEG/JXL の
  高速経路は変換経路との画素比較テストで確認する。
- **クラッシュ時の processing 残留**: 既存の単一ジョブ取得と同じ扱い。
  バッチで複数件が processing のまま残るが、再試行UIの対象は変わらない。
  必要なら別Issueで起動時回復を扱う。

## 実装の分割

1. 本設計書（docs PR）
2. core のエンコーダ・API 変更 + daemon のバッチ処理 + テスト（code PR）
3. アーキテクチャ・機能・計測手順の更新（docs PR）
