import { toast } from "svelte-sonner";
import { writeText } from "@tauri-apps/plugin-clipboard-manager";
import * as api from "$lib/api";
import type {
  AiInfo,
  ChatToolCall,
  ChatTurn,
  ConnectionInfo,
  QueryResult,
} from "$lib/api";
import { merge3 } from "$lib/mergeText";

const AUTO_SAVE_DELAY_MS = 1000;

/// 開いているクエリファイルがアプリ外で変更されていないか調べる間隔 (ms)。
const FILE_WATCH_INTERVAL_MS = 2500;

/// 結果タブの上限。超過時は最も古い非ピン留めタブを破棄する
const MAX_RESULT_TABS = 10;

/// 結果ペインの 1 タブ分の状態。結果セットに加えて
/// 「何を・どこで・いつ実行したか」を保持し、タブから再実行できるようにする
export interface ResultTab {
  id: number;
  pinned: boolean;
  sql: string;
  connection: string;
  schema: string | null;
  /// 実行開始時刻 (epoch ms)
  executedAt: number;
  result: QueryResult | null;
  error: string | null;
  /// キャンセル要求で実行が中断された (エラーとは別の見た目で表示する)
  cancelled: boolean;
  running: boolean;
  /// AI にエラー修正案を問い合わせ中 (スピナー表示・二重実行防止)
  fixing: boolean;
  /// AI が返した修正案の SQL (無ければ null)。自動実行はせず、
  /// ユーザーの Apply でエディタに挿入する
  fixSuggestion: string | null;
}

/// AI チャットペインに表示する 1 メッセージ。
/// LLM へ送るのは role / content だけで、それ以外は表示用の付随情報。
export interface ChatMessage {
  id: number;
  role: "user" | "assistant";
  content: string;
  /// この応答を作る過程でエージェントが実行した読み取りクエリ (assistant のみ)
  toolCalls?: ChatToolCall[];
  /// 応答の取得に失敗した (エラー文言を content に入れて赤く表示する)
  failed?: boolean;
}

/// タブの SQL が EXPLAIN 由来かを判定する (Analyze with AI ボタンの表示用)。
/// Explain ボタンで組み立てた SQL は必ず EXPLAIN で始まるため、
/// 先頭キーワードの一致で判定する (手入力の EXPLAIN も対象になる)
export const isExplainSql = (sql: string): boolean =>
  sql.trimStart().toLowerCase().startsWith("explain");

/// 1 つの開いているエディタタブ。タブはグローバル (全接続横断) に一列で並び、
/// 各タブが自分の接続を保持する。タブをアクティブにすると、その接続へ切り替わる
/// (engine / スキーマ / 補完 / 実行はすべてアクティブタブの接続で駆動される)。
export interface EditorTab {
  id: number;
  connection: string;
  file: string;
  content: string;
  /// 未保存の編集があるか (自動保存で false に戻る)
  dirty: boolean;
  /// ディスク上にあると分かっている内容 (読込時 = 読んだ内容、保存時 = 書いた内容)。
  /// 外部変更の検知 (この値と実ファイルを突き合わせる) と 3-way マージの base に使う。
  diskContent: string;
  /// 外部変更と手元の未保存編集が自動マージ不能な衝突状態にあるか。true の間は
  /// 「閉じる」「アプリ終了」等での自動保存を抑止し、手元の編集で外部変更を黙って
  /// 上書きするのを防ぐ (警告文の "reopen the file to discard" を実挙動と一致させる)。
  /// ユーザーが編集を続けるか明示保存すれば false に戻り、通常の上書き保存に従う。
  conflicted?: boolean;
}

let connections = $state<ConnectionInfo[]>([]);
let selectedConnection = $state<string | null>(null);
/// Writable スイッチ。false (既定) の間は SELECT/SHOW 等の副作用の無い
/// 文しか実行できない (バックエンドが強制)。事故防止のためセッションごとに
/// OFF から始め、永続化しない (再起動で勝手に書き込み可にはしない)。
let writable = $state(false);
let files = $state<string[]>([]);
let editorTabs = $state<EditorTab[]>([]);
let activeEditorTabId = $state<number | null>(null);
let resultTabs = $state<ResultTab[]>([]);
let activeTabId = $state<number | null>(null);
let errorMessage = $state<string | null>(null);
let loadingConnections = $state(false);
let schemas = $state<string[]>([]);
let activeSchema = $state<string | null>(null);
/// AI 設定の情報 (未取得・取得失敗時は null)
let aiInfo = $state<AiInfo | null>(null);
/// AI 設定の解決エラー (不明 provider 等。ボタンの title で案内する)
let aiError = $state<string | null>(null);
/// AI で SQL 生成中 (ボタンのスピナー表示・二重送信防止)
let aiGenerating = $state(false);
/// AI で実行計画を解説中 (ボタンのスピナー表示・二重送信防止)
let aiAnalyzing = $state(false);
/// AI による実行計画解説の Markdown (モーダル表示中のみ非 null)
let aiAnalysis = $state<string | null>(null);
/// AI で選択 SQL を解説中 (ボタンのスピナー表示・二重送信防止)
let aiExplaining = $state(false);
/// AI による選択 SQL 解説の Markdown (モーダル表示中のみ非 null)
let aiExplanation = $state<string | null>(null);
/// AI チャット (右ペイン) の表示中メッセージ。
/// 接続ごとにスキーマが変わるため、接続を切り替えたら破棄する。
let chatMessages = $state<ChatMessage[]>([]);
/// 応答待ちの往復が属するチャット世代 (待機中でなければ null)。
/// 会話を破棄した後も古い往復のスピナーが新しい会話に残らないよう、
/// 表示上の「送信中」は現在の世代と一致する時だけとする (chatSending)。
let chatSendingGen = $state<number | null>(null);
/// 応答待ちの往復を実行している接続 → その往復のリクエスト ID の集合。
/// クエリ実行と同じく「実行中の接続」として扱い、エディタタブが無いだけで
/// トンネル / プールが切られる (maybeDisconnectIfIdle) のを防ぐ。
/// 会話を破棄すると古い往復が走ったまま次の送信を許すため、同じ接続で
/// 複数本が同時に走りうる。どれを中断するかを指定できるよう、本数ではなく
/// ID の集合で持つ (Map は毎回作り直して反応性を保つ)。
let chatRunningConnections = $state<Map<string, Set<string>>>(new Map());

/// チャットの往復に付ける ID の採番 (プロセス内で一意なら十分)。
let nextChatRequestSeq = 1;

/// 進行中の「会話の破棄 → 中断の到達待ち → バックエンドの切替」の本数。
/// この間は新しい送信を受け付けない: 中断要求は「その時点の実行中 ID」を
/// 対象にするため、待っている隙に送られた往復は中断されないまま、
/// 切替後のプールを古いプロンプトで使ってしまう。
/// 真偽値だと、遷移が重なった時 (スキーマを続けて切り替えた等) に先に
/// 終わった方が後続の分まで解除してしまうためカウンタで持つ。
let chatTransitions = $state(0);
/// SQL 補完用のテーブル名 → カラム名リストのマップ (未取得・取得失敗は null)
let schemaMap = $state<Record<string, string[]> | null>(null);
/// 危険な文 (allow_dangerous_statements 有効な接続) の実行前確認ダイアログ。
/// 非 null の間モーダルを表示し、ユーザーの応答を resolve へ渡す
let dangerousConfirm = $state<{
  reason: string;
  resolve: (ok: boolean) => void;
} | null>(null);

// 結果タブ ID の連番 (セッション内で一意なら十分なので永続化しない)
let nextTabId = 1;
// エディタタブ ID の連番
let nextEditorTabId = 1;
/// 接続ごとに最後にアクティブだったエディタタブ ID を覚え、接続へ戻った時に復元する
const lastActiveTabByConnection = new Map<string, number>();

const getActiveEditorTab = (): EditorTab | null =>
  editorTabs.find((t) => t.id === activeEditorTabId) ?? null;

/// 実行中の loadSchemaMap の世代番号。接続・スキーマの連続切替で
/// 古い応答が後から解決しても、最新の要求の結果だけを反映するために使う
let schemaMapGeneration = 0;

/// 実行中の applyConnectionContext の世代番号。接続の連続切替で、遅い接続の
/// 応答が後から解決して新しい接続の files / schemas / activeSchema を上書き
/// しないよう、コミット前に最新世代かを検査する (schemaMapGeneration と同趣旨)
let connectionContextGeneration = 0;

/// ファイル/タブのナビゲーション世代。ファイルを開く (selectFile) / タブをアクティブに
/// する (activateEditorTab) たびに進める。await を挟む処理が、その間にユーザーが別の
/// ファイル/タブへ移動していないかを検査するのに使う (connectionContextGeneration は
/// 接続切替しか捕捉しないため、同一接続内のファイル移動はこちらで見る)。
let navigationGeneration = 0;

let autoSaveTimer: ReturnType<typeof setTimeout> | null = null;
/// 自動保存が予約されているエディタタブ ID (デバウンス中の対象)
let autoSavePendingTabId: number | null = null;

const toErrorMessage = (e: unknown): string =>
  typeof e === "string" ? e : e instanceof Error ? e.message : String(e);

/// 指定接続に対する実効 Writable。Writable スイッチはツールバーに 1 つで、
/// 現在選択中の接続の状態を表すため、別接続 (別接続タブの再実行など) では
/// 常に false (読み取り専用) になる。実行ガードと危険文確認の両方でこの値を使い、
/// 「トグルが示す接続にだけ書き込みを許可する」意味を一貫させる。
const effectiveWritable = (connection: string): boolean =>
  connection === selectedConnection && writable;

const loadConnections = async () => {
  loadingConnections = true;
  errorMessage = null;
  try {
    connections = await api.getConnections();
    // 接続設定の解決結果はバックエンドにキャッシュ済みなので、
    // AI 設定の取得はここでは軽い (取得コマンドの再実行は起きない)
    await loadAiInfo();
  } catch (e) {
    errorMessage = toErrorMessage(e);
    connections = [];
  } finally {
    loadingConnections = false;
  }
};

/// AI 設定の情報 (configured / model) を取得する。
/// 未設定は configured: false で返り、設定の解決エラー
/// (不明 provider 等) は aiError に入れて AI ボタンの title で案内する。
const loadAiInfo = async () => {
  try {
    aiInfo = await api.getAiInfo();
    aiError = null;
  } catch (e) {
    aiInfo = null;
    aiError = toErrorMessage(e);
  }
};

/// 接続設定を再読込する (プール・SSH トンネルも破棄される)。
/// 旧設定の選択状態を残さないよう一旦クリアし、同名の接続が
/// まだ存在する場合のみ再選択する (ファイル一覧も新設定で再取得される)。
/// 失敗した場合は false を返す (errorMessage 設定済み)。
const reloadConnections = async (): Promise<boolean> => {
  // リロードは全エディタタブを破棄する。衝突タブは saveAllDirtyTabs で保存されない
  // (外部変更を黙って上書きしないため) ので、そのまま進めると手元の編集が保存も
  // 明示的な破棄もされないまま失われる。衝突が残っている間はリロードを中断し、
  // ユーザーに解消 (保存で上書き / reopen で破棄) を促す。
  const conflicted = editorTabs.find((t) => t.conflicted);
  if (conflicted) {
    errorMessage =
      `Cannot reload config while "${conflicted.file}" has an unresolved ` +
      `external-change conflict. Resolve it with Overwrite or Discard in the ` +
      `editor toolbar, then reload.`;
    return false;
  }
  // タブを破棄するので、pending だけでなく全ての未保存タブを先に保存する
  if (!(await saveAllDirtyTabs())) {
    return false;
  }
  // 応答待ちのチャットは resetConnections の**前に**中断し、要求が届くまで
  // 待つ。後回し / 投げっぱなしだと、リロード前の接続設定を握ったままの
  // エージェントが破棄済みのプールを開き直し、古い認証情報 / スキーマで
  // ツール実行を続けうる
  await clearChatAndWait();
  try {
    await api.resetConnections();
  } catch (e) {
    endChatTransition();
    errorMessage = toErrorMessage(e);
    return false;
  }
  // バックエンドの入れ替えは済んだので、チャットの遷移は終わり
  // (以降に送られる往復は新しい設定で動く)
  endChatTransition();
  const previousConnection = selectedConnection;
  selectedConnection = null;
  // 設定リロードで接続が入れ替わるため、Writable も安全側 (OFF) へ戻す
  writable = false;
  files = [];
  // 設定が丸ごと入れ替わるため、開いているエディタタブを全て破棄する
  if (autoSaveTimer) {
    clearTimeout(autoSaveTimer);
    autoSaveTimer = null;
  }
  autoSavePendingTabId = null;
  // reset 前後に接続切替 (applyConnectionContext) が in-flight でも、その古い
  // 応答が後から commit して stale な接続を復活させないよう世代を進める
  connectionContextGeneration++;
  editorTabs = [];
  activeEditorTabId = null;
  lastActiveTabByConnection.clear();
  // 設定リロードでバックエンドは全プール/トンネルを破棄する (resetConnections)。
  // 確立済みフラグもクリアし、次の契機で張り直せるようにする。
  resourcesLoaded.clear();
  // リロードは全接続のプールを破棄するため、in-flight のスキーマ取り込みが (どの接続の
  // ものであれ) 破棄済みプールを「確立済み」として復活登録しないよう、全体リセット世代を
  // 進める。接続個別の世代 (connectionLifecycleGen) では選択中でない接続の in-flight を
  // 取りこぼすため、リセットは全接続共通のこの世代で見る。
  connectionsResetGen++;
  // 設定が丸ごと入れ替わるため、ピン留め含め全タブを破棄する
  resultTabs = [];
  activeTabId = null;
  schemas = [];
  activeSchema = null;
  aiInfo = null;
  aiError = null;
  aiAnalysis = null;
  aiExplanation = null;
  // チャットの中断と破棄は resetConnections の前に済ませてある (clearChat)
  // 実行中の取得が後から古いマップを書き込まないよう世代を進めて破棄する
  schemaMapGeneration++;
  schemaMap = null;
  await loadConnections();
  if (errorMessage) {
    return false;
  }
  if (
    previousConnection &&
    connections.some((c) => c.name === previousConnection)
  ) {
    await selectConnection(previousConnection);
  }
  return true;
};

/// SQL 補完用のスキーママップをバックグラウンドで再取得する。
/// 補完はあくまで補助機能のため、失敗しても通知せず補完なしで続行する。
const loadSchemaMap = async () => {
  const connection = selectedConnection;
  const generation = ++schemaMapGeneration;
  if (!connection) {
    schemaMap = null;
    return;
  }
  // 取得中に古いスキーマの候補を出さないよう先にクリアする
  schemaMap = null;
  try {
    const map = await api.getSchemaMap(connection);
    // より新しい要求が始まっていたら、古い応答は捨てる
    if (generation === schemaMapGeneration) {
      schemaMap = map;
    }
  } catch {
    // 補完なしで黙って続行 (toast も errorMessage も出さない)
  }
};

// 保留中の自動保存を確定させる。保存に失敗した場合は false を返す。
// 呼び出し元は false の時に画面遷移を中断し、未保存の編集を守ること。
const flushPendingSave = async (): Promise<boolean> => {
  if (autoSaveTimer) {
    clearTimeout(autoSaveTimer);
    autoSaveTimer = null;
  }
  const pendingId = autoSavePendingTabId;
  autoSavePendingTabId = null;
  // デバウンス予約のタブ (= 直近まで編集していたタブ) だけを確定させる。
  // 他タブの保存失敗でナビゲーションを巻き込まないよう、対象は 1 タブに限る
  // (エディタタブは接続をまたいで残るため、切替で内容が失われることはない)。
  if (pendingId == null) {
    return true;
  }
  const tab = editorTabs.find((t) => t.id === pendingId);
  if (tab && tab.dirty) {
    return saveEditorTab(tab);
  }
  return true;
};

/// 全ての dirty なエディタタブを保存する (best-effort)。全て成功したら true。
/// タブを破棄する前 (reloadConnections) に呼び、未保存の編集を失わないようにする。
/// 自動保存に失敗して pending が外れた dirty タブもここで確実に対象になる。
const saveAllDirtyTabs = async (): Promise<boolean> => {
  if (autoSaveTimer) {
    clearTimeout(autoSaveTimer);
    autoSaveTimer = null;
  }
  autoSavePendingTabId = null;
  let ok = true;
  for (const tab of editorTabs) {
    // 衝突タブは保存しない (外部変更を手元の編集で黙って上書きしないため)。
    // ユーザーが編集継続 / 明示保存で衝突を解いた場合のみ通常の dirty として保存される。
    if (tab.dirty && !tab.conflicted) {
      if (!(await saveEditorTab(tab))) {
        ok = false;
      }
    }
  }
  return ok;
};

/// 指定した接続のファイル一覧・スキーマ・補完マップを読み込み、接続コンテキストを
/// 切り替える (エディタタブには触れない)。接続選択・タブアクティブ化の両方から使う。
///
/// 重要: `selectedConnection` は読み込みを終えてから **最後にまとめて** 反映する。
/// 先に `selectedConnection = name` すると、await 中は「接続は新 (name) だがエディタは
/// まだ旧タブの SQL を表示中」というズレが生じ、その窓で Run すると旧 SQL が新接続で
/// 走ってしまう (DB クライアントとして致命的)。呼び出し側の `activeEditorTabId` 反映は
/// この関数の resolve 直後の同一マイクロタスクで行われるため、コミット〜タブ反映の間に
/// ユーザー操作 (マクロタスク) は割り込めず、接続とタブは常に整合する。
///
/// コミットできたら true。連続切替でより新しい要求に追い越された場合は、何も反映せず
/// false を返す (呼び出し側は activeEditorTabId を触らずに中断する)。
const applyConnectionContext = async (name: string): Promise<boolean> => {
  const generation = ++connectionContextGeneration;
  const defaultSchema = connections.find((c) => c.name === name)?.schema ?? null;
  let loadedFiles: string[] = [];
  let filesError: string | null = null;
  try {
    loadedFiles = await api.listQueryFiles(name);
  } catch (e) {
    filesError = toErrorMessage(e);
  }
  // await の間により新しい切替が始まっていたら、この応答は捨てる (上書き防止)
  if (generation !== connectionContextGeneration) {
    return false;
  }
  // アクティブスキーマの取得は接続を張らない (schema_override か設定値を返すだけ)
  // ため選択時に行ってよい。スキーマ一覧 (listSchemas) は接続を張るので、
  // ここでは取得しない (「選択した瞬間にトンネルが開く」のを避ける)。
  let schema = defaultSchema;
  try {
    schema = (await api.getActiveSchema(name)) ?? schema;
  } catch {
    // getActiveSchema は接続を張らないが、念のため失敗時は既定スキーマのままにする
  }
  if (generation !== connectionContextGeneration) {
    return false;
  }
  // ここから resolve まで await を挟まず、接続コンテキストを一括反映する。
  // 別の接続へ切り替わったら Writable を安全側 (OFF) へ戻す。ある接続で
  // 書き込みを許可したまま別接続 (本番など) に移り、誤って書き込む事故を防ぐ。
  // 同一接続の再選択 (同接続のエディタタブ切替など) では維持する。
  if (selectedConnection !== name) {
    writable = false;
    // AI チャットの会話も破棄し、走っているエージェントを中断する。
    // system prompt に載るスキーマが接続ごとに違うため会話は引き継げず、
    // 破棄するだけではバックエンドが切替前の接続でツール実行を続ける
    clearChat();
  }
  selectedConnection = name;
  errorMessage = filesError;
  files = filesError ? [] : loadedFiles;
  activeSchema = schema;
  // スキーマ一覧・補完マップは接続を張るため、選択時点では取得しない
  // (「選択した瞬間にトンネルが開く」のを避ける)。プルダウンは現在のスキーマのみを
  // 出しておき、ファイルをエディタに読み込んだ時 / スキーマブラウザを開いた時に
  // ensureConnectionResources で全一覧・補完マップを取り込む。
  schemas = schema ? [schema] : [];
  // 補完候補は新接続向けに一旦クリアする (世代も進め、古い取得の後追い書き込みを防ぐ)
  schemaMapGeneration++;
  schemaMap = null;
  if (resourcesLoaded.has(name)) {
    // 既に確立済みの接続を選び直した場合、トンネルは開いたままなので、上でリセットした
    // スキーマ一覧・補完マップを取り直して UI (プルダウン / 補完) を復元する。新規に
    // トンネルを開くわけではない (「選択した瞬間に開く」方針には反しない)。
    // resourcesLoaded は外さない — 外して非同期で再登録すると、その隙に別接続へ
    // 切り替えた時に切断判定 (maybeDisconnectIfIdle) や再登録と競合し、エディタタブの
    // 無いトンネルが貼りっぱなしになりうるため。確立状態は保ったまま UI だけ取り直す。
    void loadConnectionSchemaResources(name);
  }
  return true;
};

/// 接続を確立済み (トンネル/プールを開き、スキーマ一覧・補完マップを取り込み済み)
/// の接続名。ensureConnectionResources で登録し、disconnect / reset で外す。
/// これを接続状態の真実として使い、キャッシュ (schemas 等) の有無で代用しない
/// (切断後もキャッシュは残るため、代用すると再オープンの契機を取りこぼす)。
const resourcesLoaded = new Set<string>();

/// 接続ごとの「切断 / リセットが起きた回数」。スキーマ取り込み (listSchemas は非同期で
/// その間にトンネルが張られる) を待つ間に、その接続が切断されたかを検知するために使う。
/// 取り込み開始時の値を控え、完了後に値が変わっていたら「待っている間に切断された」と分かる
/// (状態 (selectedConnection / editorTabs) だけでは、選択中のまま最後のタブを閉じて切断された
/// 接続と、スキーマブラウザだけで生きている選択中の接続を区別できないため、イベント回数で見る)。
const connectionLifecycleGen = new Map<string, number>();
const bumpConnectionLifecycle = (connection: string) => {
  connectionLifecycleGen.set(
    connection,
    (connectionLifecycleGen.get(connection) ?? 0) + 1,
  );
};

/// 設定リロード (reloadConnections → resetConnections) は全接続のプール/トンネルを一括
/// 破棄する。個別接続の世代では「選択中でない接続の in-flight 取り込み」を取りこぼすため、
/// リセットは全接続共通のこの世代で検知する。取り込み開始時に控え、完了後に進んでいれば
/// 「待っている間に全体リセットされた」と分かる。
let connectionsResetGen = 0;

/// 接続のスキーマ一覧・補完マップを取り込んで UI (プルダウン / 補完) へ反映する。
/// listSchemas がプールを取得する (= トンネルを張る) 契機。listSchemas が成功し、かつ
/// 取り込み中に切断が挟まらなかった場合に true を返す (呼び出し側の resourcesLoaded 管理に
/// 使う)。取り込み中に別接続へ切り替わっていたら反映はスキップする (selectedConnection
/// ガード)。resourcesLoaded 自体はここでは触らない — 「確立済みか」の判定と「取り込み中か」を
/// 分離し、再取り込み中に切替が走ってもライフサイクル管理 (maybeDisconnectIfIdle) と
/// 競合しないようにするため。
const loadConnectionSchemaResources = async (
  connection: string,
): Promise<boolean> => {
  const gen = connectionLifecycleGen.get(connection) ?? 0;
  const resetGen = connectionsResetGen;
  let loaded: string[];
  try {
    loaded = await api.listSchemas(connection);
  } catch {
    // 接続失敗などは致命的でない。プルダウンは現在値のみのままにし、次の契機で再試行。
    return false;
  }
  // 取り込みを待つ間に切断 (最後のエディタタブを閉じた / 別接続へ切替) や設定リロードが
  // 挟まっていたら、この接続はもう生きていない。listSchemas がトンネルを開き直している
  // 場合に備えて閉じ直し (アイドルなら)、UI 反映も確立済み登録もしない。これがないと
  // アイドルな接続が「確立済み」として残り、遅延ライフサイクルが崩れる。切断は接続個別の
  // 世代で、設定リロード (全接続破棄) は全体リセット世代で検知する。
  if (
    (connectionLifecycleGen.get(connection) ?? 0) !== gen ||
    connectionsResetGen !== resetGen
  ) {
    maybeDisconnectIfIdle(connection);
    return false;
  }
  if (selectedConnection === connection) {
    schemas = loaded;
    // 補完マップも取り込む (同じプールを使うので追加のトンネルは張らない。
    // 失敗しても補完なしで続行)。
    void loadSchemaMap();
  }
  return true;
};

/// トンネル / 接続を実際に開くべき契機 (エディタにファイルを読み込んだ時・
/// スキーマブラウザを開いた時) で呼ぶ。接続を確立してスキーマ一覧と補完マップを
/// 取り込む。接続選択だけでは呼ばない (選択した瞬間にトンネルが開くのを避ける)。
/// 既に確立済み (resourcesLoaded) なら何もしない。切断されると外れるため、
/// 全エディタを閉じて切断した後に再度開けば、ここで確実に張り直す。
const ensureConnectionResources = async (connection: string) => {
  if (connection !== selectedConnection || resourcesLoaded.has(connection)) {
    return;
  }
  // 成功前に登録すると、失敗接続を「確立済み」と誤認して再試行を取りこぼすため、
  // listSchemas (これが接続を張る) が成功してから登録する。取り込み中に切断が挟まった
  // 場合は loadConnectionSchemaResources が false を返す (+ 開き直したトンネルを閉じ直す)
  // ので、その時は登録しない — アイドルな接続を「確立済み」と誤認しない。
  if (await loadConnectionSchemaResources(connection)) {
    resourcesLoaded.add(connection);
  }
};

/// この接続がもう不要 (エディタタブが 1 つも無く、実行中のクエリ / セル編集も無い)
/// なら、SSH トンネル / プールを破棄する。エディタタブを閉じた時とクエリ完了時に
/// 呼ぶ。実行中のクエリがある間は破棄しない (トンネルを途中で切ると実行中クエリの
/// コネクションが壊れるため)。schema_override はバックエンドに残るので、張り直し後も
/// 同じアクティブスキーマで繋がる。次にファイルを開く / スキーマブラウザを開く /
/// クエリを実行した時に自動で張り直される。
const maybeDisconnectIfIdle = (connection: string) => {
  if (editorTabs.some((t) => t.connection === connection)) {
    return;
  }
  if (isConnectionRunning(connection)) {
    return;
  }
  resourcesLoaded.delete(connection);
  // 切断イベントを記録する。この接続のスキーマ取り込みが in-flight なら、完了時に
  // 「待っている間に切断された」と検知して確立済み登録を防ぐ (defeat lazy lifecycle の回避)。
  bumpConnectionLifecycle(connection);
  // fire-and-forget。失敗しても致命的でない (次に接続を張り直す時に上書きされる) ため
  // 未処理 rejection にしないよう握り潰す。
  void api.disconnect(connection).catch(() => {});
};

/// 接続に紐づくエディタタブのうち、アクティブに復元すべきものを選ぶ。
/// 直近にアクティブだったタブを優先し、無ければ最後に開いたタブ、無ければ null。
const pickTabForConnection = (name: string): number | null => {
  const remembered = lastActiveTabByConnection.get(name);
  if (
    remembered != null &&
    editorTabs.some((t) => t.id === remembered && t.connection === name)
  ) {
    return remembered;
  }
  for (let i = editorTabs.length - 1; i >= 0; i--) {
    if (editorTabs[i].connection === name) {
      return editorTabs[i].id;
    }
  }
  return null;
};

const selectConnection = async (name: string) => {
  if (name === selectedConnection) {
    // 現在の接続を再選択したら、進行中の別接続への切替 (applyConnectionContext は
    // commit まで selectedConnection を変えないため、その最中は現接続が選択中に
    // 見える) をキャンセルする。世代を進めておくと in-flight の切替は commit されず、
    // 「今の接続に留まる」という操作の意図どおりになる。
    connectionContextGeneration++;
    return;
  }
  const previous = selectedConnection;
  // 未保存タブは best-effort で保存するが、保存失敗でも切替は止めない。
  // エディタタブは接続をまたいで残るため、切替で内容が失われることはない
  // (書込不可などで保存に失敗しても、dirty のままタブに保持される)。
  await flushPendingSave();
  // 結果タブ・エディタタブは接続をまたいで残す (接続切替では破棄しない)。
  // より新しい切替に追い越されたら、タブ選択を触らず中断する。
  if (!(await applyConnectionContext(name))) {
    return;
  }
  // この接続で最後に開いていたタブを復元する (無ければエディタは空表示)
  activeEditorTabId = pickTabForConnection(name);
  // 切替元の接続が「エディタタブを 1 つも持たない」なら、その接続はもう不要と判断して
  // トンネル / プールを閉じる。スキーマブラウザ (TABLES) を開いただけの接続は
  // エディタタブを持たず removeEditorTab / executeTab を通らないため、ここで閉じないと
  // 張りっぱなしになる。エディタタブを持つ接続は maybeDisconnectIfIdle が no-op になり、
  // 切替後も開いたまま残る (「作ったトンネルは貼りっぱなし」の方針どおり)。
  if (previous) {
    maybeDisconnectIfIdle(previous);
  }
};

/// エディタタブをアクティブにする。タブの接続が現在の接続と違えば、その接続へ
/// 切り替える (files / スキーマ / 補完もタブの接続のものに揃える)。
const activateEditorTab = async (id: number) => {
  if (id === activeEditorTabId) {
    return;
  }
  const tab = editorTabs.find((t) => t.id === id);
  if (!tab) {
    return;
  }
  // タブのアクティブ化もナビゲーション。await を挟む処理が「その後にユーザーが
  // 別タブへ移動した」ことを検知できるよう世代を進める。
  navigationGeneration++;
  // 未保存タブは best-effort で保存するが、保存失敗でもアクティブ化は止めない。
  // (書込不可などで保存に失敗しても未保存 SQL を閲覧・コピーできるようにする。
  //  タブは残るので内容は失われない)
  await flushPendingSave();
  const previous = selectedConnection;
  if (tab.connection !== selectedConnection) {
    // より新しい切替に追い越されたら、このタブをアクティブにしない
    if (!(await applyConnectionContext(tab.connection))) {
      return;
    }
    // タブのアクティブ化も接続を切り替える経路。切替元がスキーマブラウザだけで開いた
    // (エディタタブの無い) 接続なら、selectConnection と同様にここでも閉じる。
    // このパスを塞がないと、TABLES だけ開いた接続のトンネルが貼りっぱなしになる。
    if (previous) {
      maybeDisconnectIfIdle(previous);
    }
  }
  activeEditorTabId = id;
  lastActiveTabByConnection.set(tab.connection, id);
};

// アクティブスキーマ (database) を切り替える。成功したら true。
const changeActiveSchema = async (schema: string): Promise<boolean> => {
  const connection = selectedConnection;
  if (!connection || schema === activeSchema) {
    return true;
  }
  // 未保存タブは best-effort で保存 (失敗してもスキーマ切替は止めない)
  await flushPendingSave();
  // AI チャットの会話は切替の**前に**破棄し、中断が届くまで待つ。
  // system prompt に載るスキーマが変わるため会話は引き継げず、切替が
  // 先行すると応答待ちの往復が古いプロンプトのまま新しいスキーマの
  // プールでクエリを実行してしまう
  await clearChatAndWait();
  try {
    await api.setActiveSchema(connection, schema);
    // 切替中に別接続へ移っていたら、そのスキーマ表示を新接続に適用しない
    if (selectedConnection !== connection) {
      return false;
    }
    activeSchema = schema;
    errorMessage = null;
    // 切替先スキーマの補完候補をバックグラウンドで取得する (待たない)
    void loadSchemaMap();
    return true;
  } catch (e) {
    errorMessage = toErrorMessage(e);
    return false;
  } finally {
    // 成否によらず遷移を終える (失敗時に入力が塞がったままにならないように)
    endChatTransition();
  }
};

/// ファイルを開く。既に開いているタブがあればアクティブにし、無ければ
/// 内容を読み込んで新しいタブを作りアクティブにする (FilesPane から呼ばれる)。
const selectFile = async (fileName: string) => {
  // このファイルオープンをナビゲーション世代として記録する (await 中に別ファイル/タブへ
  // 移動されたかを後で検査するため)。
  const navGen = ++navigationGeneration;
  // 読み込み先の接続を await 前に固定する。読込中に接続が切り替わっても、
  // タブは必ず「内容を読んだ接続」に紐づける (誤った接続への実行/保存を防ぐ)。
  const connection = selectedConnection;
  if (!connection) {
    return;
  }
  const existing = editorTabs.find(
    (t) => t.connection === connection && t.file === fileName,
  );
  if (existing) {
    // 衝突状態のタブを開き直した場合は、警告文 "reopen the file to discard them" に
    // 従い手元の編集を破棄してディスクの内容を読み直す (best-effort。失敗時は現状維持)。
    if (existing.conflicted) {
      let disk: string | undefined;
      try {
        disk = await api.readQueryFile(connection, fileName);
      } catch (e) {
        errorMessage = toErrorMessage(e);
      }
      // 読込 await 中にユーザーが別接続へ移動、または同一接続内で別ファイル/タブへ
      // 移動していたら、この結果でタブを書き換えず、フォーカスも奪わない (遅い読込が
      // 新しいナビゲーションを巻き戻さないようにする。未着タブ生成パスの stale-read
      // ガードと同方針)。同一接続内のファイル移動は connection 比較では捕まらないため
      // ナビゲーション世代で検査する。
      if (selectedConnection !== connection || navigationGeneration !== navGen) {
        return;
      }
      if (disk !== undefined) {
        existing.content = disk;
        existing.diskContent = disk;
        existing.dirty = false;
        existing.conflicted = false;
        cancelPendingSaveFor(existing.id);
        conflictNotified.delete(existing.id);
        toast.info(`Reloaded "${fileName}" (discarded unsaved edits)`);
      }
    }
    await activateEditorTab(existing.id);
    // ファイルをエディタで開いた = 接続を使う契機。トンネル/接続を張り直す
    // (全エディタを閉じて切断した後に再度開いた場合もここで復帰する)。
    void ensureConnectionResources(connection);
    return;
  }
  // 未保存タブは best-effort で保存 (失敗してもファイルオープンは止めない)
  await flushPendingSave();
  try {
    const content = await api.readQueryFile(connection, fileName);
    // 読込中に別接続へ切り替わっていたら、この読込結果は捨てる
    // (ユーザーはもうその接続を見ていないので、開かない)
    if (selectedConnection !== connection) {
      return;
    }
    const tab: EditorTab = {
      id: nextEditorTabId++,
      connection,
      file: fileName,
      content,
      dirty: false,
      diskContent: content,
    };
    editorTabs = [...editorTabs, tab];
    activeEditorTabId = tab.id;
    lastActiveTabByConnection.set(connection, tab.id);
    errorMessage = null;
    // ファイルをエディタに読み込んだ = 接続を使う契機。ここでトンネル/接続を張り、
    // スキーマ一覧と補完マップを取り込む (接続選択時点では張っていない)。
    void ensureConnectionResources(connection);
  } catch (e) {
    errorMessage = toErrorMessage(e);
  }
};

/// deep link (`queryfolio://open/<path>`) / CLI で指定されたファイルを開く。
/// 対象接続へ切り替えてからそのファイルを開く。接続が設定に無ければエラー表示。
/// (バックエンドが保存領域配下かを検証済みの connection / fileName を受け取る)
const openFileByTarget = async (connection: string, fileName: string) => {
  // 実行中インスタンスへの deep link は、まれに接続一覧の初回ロード完了前に
  // 届き得る。空のまま「接続が無い」と捨てないよう、未ロードなら先に読み込む。
  if (connections.length === 0) {
    await loadConnections();
  }
  if (!connections.some((c) => c.name === connection)) {
    errorMessage = `Connection '${connection}' is not defined in the config`;
    return;
  }
  // selectConnection は同じ接続なら no-op (files は既に読込済み)、別接続なら
  // 切り替えてファイル一覧を読み込む。追い越された場合は selectedConnection が
  // 変わるため、下のガードで開かない。
  await selectConnection(connection);
  if (selectedConnection !== connection) {
    return;
  }
  await selectFile(fileName);
};

/// エディタタブを閉じる。未保存なら閉じる前に保存する (best-effort)。
/// アクティブタブを閉じたら右隣 (無ければ左隣) をアクティブにする。
const removeEditorTab = async (id: number, save: boolean) => {
  const tab = editorTabs.find((t) => t.id === id);
  if (!tab) {
    return;
  }
  // 衝突タブ (外部変更 vs 手元の未保存編集) を閉じる時は、どちらかを黙って失わない。
  // 通常の dirty タブは閉じる前に保存されるが、衝突タブを保存すると外部変更を黙って
  // 上書きし、逆に破棄すると手元の編集を黙って失う。どちらを捨てるかはユーザーの
  // 明示操作 (保存で上書き / reopen で破棄) に委ねるべきなので、閉じる操作はブロックし
  // トーストで解消を促す。save=false (ファイル削除で閉じる等) は保存も上書きもしないため対象外。
  if (save && tab.conflicted) {
    toast.warning(
      `"${tab.file}" has unsaved edits conflicting with an external change`,
      {
        description:
          "Resolve it first with Overwrite or Discard in the editor toolbar, then close.",
      },
    );
    return;
  }
  // このタブの自動保存予約が残っていれば解除する (閉じた後に走らせない)
  if (autoSavePendingTabId === id) {
    if (autoSaveTimer) {
      clearTimeout(autoSaveTimer);
      autoSaveTimer = null;
    }
    autoSavePendingTabId = null;
  }
  // 通常の dirty タブは閉じる前に保存する (衝突タブは上でブロック済みなのでここには
  // 到達しない)。
  if (save && tab.dirty) {
    // 保存に失敗したら閉じない (未保存内容をメモリごと失わないため)
    if (!(await saveEditorTab(tab))) {
      return;
    }
    // 保存 await の間にさらに編集された場合、saveEditorTab は dirty を残す。
    // その編集を失わないよう close を中断する (自動保存タイマーが後で確定させる)
    if (tab.dirty) {
      return;
    }
  }
  // 保存 await 中に配列が変わっている可能性があるため、位置は取り直す
  const index = editorTabs.findIndex((t) => t.id === id);
  if (index < 0) {
    return;
  }
  editorTabs = editorTabs.filter((t) => t.id !== id);
  if (lastActiveTabByConnection.get(tab.connection) === id) {
    lastActiveTabByConnection.delete(tab.connection);
  }
  if (activeEditorTabId === id) {
    // filter 後、元 index の位置には右隣タブが繰り上がっている
    const neighbor = editorTabs[index] ?? editorTabs[index - 1] ?? null;
    activeEditorTabId = null;
    if (neighbor) {
      await activateEditorTab(neighbor.id);
    }
  }
  // この接続のエディタタブが全て閉じられたら、SSH トンネル / プールを閉じる。
  maybeDisconnectIfIdle(tab.connection);
};

const closeEditorTab = (id: number) => {
  void removeEditorTab(id, true);
};

const createFile = async (fileName: string) => {
  const connection = selectedConnection;
  if (!connection) {
    return;
  }
  try {
    const normalized = await api.createQueryFile(connection, fileName);
    // 作成中に別接続へ切り替わっていたら、新接続の一覧を汚さず開かない
    if (selectedConnection !== connection) {
      return;
    }
    files = await api.listQueryFiles(connection);
    await selectFile(normalized);
  } catch (e) {
    errorMessage = toErrorMessage(e);
  }
};

const deleteFile = async (fileName: string) => {
  const connection = selectedConnection;
  if (!connection) {
    return;
  }
  try {
    await api.deleteQueryFile(connection, fileName);
    // 削除したファイルの開いているタブを閉じる (ファイルは消えたので保存しない)
    const victims = editorTabs.filter(
      (t) => t.connection === connection && t.file === fileName,
    );
    for (const v of victims) {
      await removeEditorTab(v.id, false);
    }
    // タブを閉じる過程で接続が切り替わっていなければ一覧を更新する
    if (selectedConnection === connection) {
      files = await api.listQueryFiles(connection);
    }
  } catch (e) {
    errorMessage = toErrorMessage(e);
  }
};

// ファイルをリネームする。成功したら正規化後の新ファイル名、失敗したら null。
// 対象ファイルを開いているタブがあればリネーム前に保存し、成功後は追従する。
const renameFile = async (
  oldName: string,
  newName: string,
): Promise<string | null> => {
  // await をまたぐ間に接続が切り替わっても、リネームは開始時の接続に対して
  // 行う (flushPendingSave 中の接続切替による取り違えを防ぐ)
  const connection = selectedConnection;
  if (!connection) {
    return null;
  }
  // 対象ファイルを開いているタブがあれば未保存内容を先に確定させる
  const opened = editorTabs.some(
    (t) => t.connection === connection && t.file === oldName,
  );
  if (opened && !(await flushPendingSave())) {
    return null;
  }
  try {
    const normalized = await api.renameQueryFile(connection, oldName, newName);
    // リネーム中に接続が切り替わっていたら、旧接続の一覧で上書きしない
    if (selectedConnection === connection) {
      files = await api.listQueryFiles(connection);
    }
    // 開いているタブのファイル名を追従させる
    for (const t of editorTabs) {
      if (t.connection === connection && t.file === oldName) {
        t.file = normalized;
      }
    }
    errorMessage = null;
    return normalized;
  } catch (e) {
    errorMessage = toErrorMessage(e);
    return null;
  }
};

// クエリファイルを別の接続のフォルダへ移動する (FILES から CONNECTIONS への
// ドラッグ & ドロップ)。成功したら true、失敗したら errorMessage を設定して false。
//
// 移動元 (fromConnection) は**ドラッグを開始した時点の接続**を呼び出し側から
// 受け取る。selectedConnection を見に行くと、ドラッグ中に接続が切り替わった時に
// 別接続の同名ファイルを移動してしまう。
const moveFileToConnection = async (
  fileName: string,
  fromConnection: string,
  toConnection: string,
): Promise<boolean> => {
  if (fromConnection === toConnection) {
    return false;
  }
  // 対象ファイルを開いているタブがあれば、**移動する前に**未保存内容を確定させる。
  // 移動後に保存すると、移動元のパスにファイルが作り直されてしまう。
  // 移動後はタブを閉じる (下記) ので、ここで保存しそこねた編集は失われる。
  // そのためデバウンス待ちの分 (flushPendingSave) だけでなく、対象タブが dirty
  // なら明示的に保存し、保存できなければ移動そのものを中止する。
  const openedTabs = editorTabs.filter(
    (t) => t.connection === fromConnection && t.file === fileName,
  );
  if (openedTabs.length > 0) {
    if (!(await flushPendingSave())) {
      return false;
    }
    for (const tab of openedTabs) {
      if (tab.dirty && !(await saveEditorTab(tab))) {
        return false;
      }
    }
  }
  try {
    await api.moveQueryFile(fromConnection, toConnection, fileName);
  } catch (e) {
    errorMessage = toErrorMessage(e);
    return false;
  }
  // 開いていたタブは閉じる (移動でファイルはこの接続のものではなくなった)。
  // 保存は上で済ませてあるので、ここでは保存しない (save=false)。
  // 移動先の接続で開き直すのはユーザーに委ねる — タブの接続を差し替えると、
  // プール / SSH トンネルの参照 (maybeDisconnectIfIdle) と噛み合わなくなる。
  for (const tab of openedTabs) {
    await removeEditorTab(tab.id, false);
  }
  // タブを閉じる過程で接続が切り替わっていなければ一覧を更新する
  if (selectedConnection === fromConnection) {
    files = await api.listQueryFiles(fromConnection);
  }
  errorMessage = null;
  return true;
};

// クエリファイルの絶対パスをクリップボードへコピーする。成功したら true。
// 失敗時は errorMessage を設定して false を返す。
const copyFilePath = async (fileName: string): Promise<boolean> => {
  const connection = selectedConnection;
  if (!connection) {
    return false;
  }
  try {
    const path = await api.queryFilePath(connection, fileName);
    await writeText(path);
    errorMessage = null;
    return true;
  } catch (e) {
    errorMessage = toErrorMessage(e);
    return false;
  }
};

// エディタタブの内容を保存する。成功したら true。
// 失敗時は dirty を保持したまま errorMessage を設定する。
//
// force=false (既定): 暗黙の保存 (自動保存・閉じる前保存等)。バックエンドの
//   write_query_file_if_unchanged で、把握している base (diskContent) とディスクの
//   現在内容が一致する時だけ書く (楽観的排他)。食い違えば書かず、外部変更処理
//   (自動マージ or 衝突検知) に委ねる。マージが clean に通ってタブが保存済みになれば
//   true、衝突が残れば false を返す。検査と書き込みをバックエンドの単一呼び出しで
//   隣接させることで、フロント往復ぶんの TOCTOU をなくす。
// force=true: ユーザーの明示的な上書き (Overwrite ボタン)。ディスクの現在内容が
//   何であれ手元の内容で置き換える意思なので、検査せず書き込む。
//   (自動マージの書き戻しは force ではなく expectedBase=disk の CAS を直接使う。)
const saveEditorTab = async (
  tab: EditorTab,
  opts: { force?: boolean } = {},
): Promise<boolean> => {
  // 書き込み中にさらに編集された場合、その古い保存完了で新しい編集の dirty を
  // 消してはならない (lost update 防止)。保存した内容を控え、完了時に内容が
  // 変わっていない時だけ dirty を下ろす。
  const saved = tab.content;
  const expectedBase = tab.diskContent;
  try {
    if (!opts.force) {
      // バックエンドで CAS 書き込み。書けなければ外部変更あり。
      let wrote: boolean;
      try {
        wrote = await api.writeQueryFileIfUnchanged(
          tab.connection,
          tab.file,
          saved,
          expectedBase,
        );
      } catch (e) {
        errorMessage = `Failed to save the file: ${toErrorMessage(e)}`;
        return false;
      }
      if (!wrote) {
        // ディスクが base と食い違う = 外部変更あり。黙って上書きせず、外部変更処理
        // (自動マージ / 衝突検知) に委ねる。自動マージが clean に通れば tab は保存済み
        // (dirty=false) になるので、その実際の結果を返す (close/reload/rename が
        // マージ成功を「保存失敗」と誤認して中断しないように)。
        await checkTabForExternalChange(tab.id);
        const after = editorTabs.find((t) => t.id === tab.id);
        return !!after && !after.dirty && !after.conflicted;
      }
      // 書けた → 状態確定。
      tab.diskContent = saved;
      tab.conflicted = false;
      if (tab.content === saved) {
        tab.dirty = false;
      }
      return true;
    }
    await api.writeQueryFile(tab.connection, tab.file, saved);
    // 自分が書いた内容をディスクの既知内容として控える。これにより外部変更
    // ウォッチャが「自分の保存」を外部変更と誤検知しない。
    tab.diskContent = saved;
    // 明示保存 / 自動マージ保存が通った = 手元の内容がディスクに反映された。
    // 衝突状態は解消したので自動保存の抑止を外す。
    tab.conflicted = false;
    if (tab.content === saved) {
      tab.dirty = false;
    }
    return true;
  } catch (e) {
    errorMessage = `Failed to save the file: ${toErrorMessage(e)}`;
    return false;
  }
};

// アクティブなエディタタブを保存する (Toolbar 等から明示保存する場合用)。
const saveCurrentFile = async (): Promise<boolean> => {
  const tab = getActiveEditorTab();
  if (!tab) {
    return true;
  }
  return saveEditorTab(tab);
};

/// アクティブな衝突タブについて、手元の未保存編集を破棄してディスクの内容を読み直す。
/// 警告文の "reopen the file to discard" と同じ効果を、常に到達可能な UI (ツールバー)
/// から明示的に呼べるようにするための手段 (選択中ファイルの再クリックが Rename に
/// なる等で reopen 経路に到達できないケースの逃げ道)。
const discardActiveFileConflict = async (): Promise<void> => {
  const tab = getActiveEditorTab();
  if (!tab || !tab.conflicted) {
    return;
  }
  const { id, connection, file } = tab;
  let disk: string;
  try {
    disk = await api.readQueryFile(connection, file);
  } catch (e) {
    errorMessage = toErrorMessage(e);
    return;
  }
  // 読込 await 中にタブが閉じられた / 別接続へ移動した / ファイル名が変わった場合は
  // 適用しない (古い読込で現在の状態を壊さない)。
  const cur = editorTabs.find((t) => t.id === id);
  if (
    !cur ||
    selectedConnection !== connection ||
    cur.connection !== connection ||
    cur.file !== file
  ) {
    return;
  }
  cur.content = disk;
  cur.diskContent = disk;
  cur.dirty = false;
  cur.conflicted = false;
  cancelPendingSaveFor(id);
  conflictNotified.delete(id);
  toast.info(`Reloaded "${file}" (discarded unsaved edits)`);
};

/// アクティブな衝突タブについて、手元の編集でディスクを上書き保存する (ユーザーの
/// 明示的な上書き意思)。CAS を通さず force で書くため、外部変更を意図的に置き換える。
const overwriteActiveFileConflict = async (): Promise<boolean> => {
  const tab = getActiveEditorTab();
  if (!tab) {
    return true;
  }
  return saveEditorTab(tab, { force: true });
};

/// 指定タブのデバウンス自動保存を (張り直して) 予約する。既存の予約は解除される
/// (自動保存は一度に 1 タブのみを対象にする既存仕様に合わせる)。
const scheduleAutoSave = (tabId: number) => {
  autoSavePendingTabId = tabId;
  if (autoSaveTimer) {
    clearTimeout(autoSaveTimer);
  }
  autoSaveTimer = setTimeout(() => {
    autoSaveTimer = null;
    const id = autoSavePendingTabId;
    autoSavePendingTabId = null;
    const target = editorTabs.find((t) => t.id === id);
    // 衝突タブは暗黙保存しない (saveEditorTab の CAS でも守られるが、無駄な read を
    // 避けるためここでも弾く)。saveEditorTab は force 無しなので、debounce 中に
    // 外部書き込みが入っていれば書き込み前の CAS で検知し、上書きせず衝突検知へ委ねる。
    if (target && target.dirty && !target.conflicted) {
      void saveEditorTab(target);
    }
  }, AUTO_SAVE_DELAY_MS);
};

/// エディタからの変更通知。アクティブタブの内容を更新し、自動保存を
/// デバウンスして予約する。予約は編集中のタブを対象にする。
const updateEditorContent = (content: string) => {
  const tab = getActiveEditorTab();
  if (!tab || content === tab.content) {
    return;
  }
  tab.content = content;
  tab.dirty = true;
  // 衝突中は暗黙の自動保存を予約しない (外部変更を黙って上書きしないため)。
  // 編集は反映するが、解消はユーザーの明示操作 (ツールバーの Overwrite / Discard) に
  // 委ねる。編集の結果ディスク内容と一致すれば、次のウォッチャ tick が衝突を自動解消する。
  if (!tab.conflicted) {
    scheduleAutoSave(tab.id);
  }
};

/// 履歴パネル・スキーマブラウザからの SQL 断片の挿入。
/// 開いているファイルの末尾に追記する (既存の編集内容を上書きしないよう、
/// 置換ではなく追記にする)。実行はしない。挿入できたら true を返す。
/// エディタへの反映は SqlEditor 側の $effect が行う。
const insertSqlSnippet = (sql: string): boolean => {
  if (!selectedConnection) {
    toast.warning("Select a connection first");
    return false;
  }
  const tab = getActiveEditorTab();
  if (!tab) {
    toast.warning("Select or create a query file first");
    return false;
  }
  const trimmed = tab.content.replace(/\s+$/, "");
  updateEditorContent(trimmed ? `${trimmed}\n\n${sql}\n` : `${sql}\n`);
  return true;
};

/// 自然言語の指示から AI で SQL を生成し、エディタに挿入する
/// (自動実行はしない。ユーザーが内容を確認してから実行する)。
/// 成功したら true を返す (入力欄を閉じる判定に使う)。
const generateSql = async (instruction: string): Promise<boolean> => {
  if (!selectedConnection) {
    toast.warning("Select a connection first");
    return false;
  }
  if (!getActiveEditorTab()) {
    toast.warning("Select or create a query file first");
    return false;
  }
  if (!instruction.trim()) {
    toast.warning("Enter an instruction for the SQL to generate");
    return false;
  }
  if (aiGenerating) {
    return false;
  }
  aiGenerating = true;
  try {
    const sql = await api.aiGenerateSql(selectedConnection, instruction);
    if (!sql.trim()) {
      toast.warning("The AI returned an empty response");
      return false;
    }
    // 生成中に接続・ファイルの選択が外れた場合は挿入されない
    // (insertSqlSnippet が warning を出す) ため、成功時のみ通知する
    if (!insertSqlSnippet(sql)) {
      return false;
    }
    toast.success("Generated SQL inserted into the editor");
    return true;
  } catch (e) {
    toast.error("Failed to generate SQL", {
      description: toErrorMessage(e),
    });
    return false;
  } finally {
    aiGenerating = false;
  }
};

/// タブに記録された SQL とエラーメッセージから、AI に修正案を問い合わせて
/// タブへ書き込む (自動実行はしない。ユーザーが Apply でエディタに挿入する)。
const fixSqlWithAi = async (tabId: number) => {
  const tab = resultTabs.find((t) => t.id === tabId);
  if (!tab || !tab.error || !tab.sql.trim()) {
    return;
  }
  // 二重実行防止 (ボタンも disabled にしているが防御的にガードする)
  if (tab.fixing) {
    return;
  }
  // 問い合わせ中に Re-run されて別の実行結果になった場合に、
  // 古いエラーへの修正案を書き込まないよう実行時刻を控えておく
  const requestedExecutedAt = tab.executedAt;
  tab.fixing = true;
  try {
    const fixed = await api.aiFixSql(tab.connection, tab.sql, tab.error);
    // 問い合わせ中に設定再読込などでタブが破棄された・再実行で
    // 結果が入れ替わった場合は、古い修正案を捨てる
    const current = resultTabs.find((t) => t.id === tabId);
    if (!current || current.executedAt !== requestedExecutedAt) {
      return;
    }
    if (!fixed.trim()) {
      toast.warning("The AI returned an empty response");
      return;
    }
    tab.fixSuggestion = fixed;
  } catch (e) {
    toast.error("Failed to get a fix suggestion", {
      description: toErrorMessage(e),
    });
  } finally {
    tab.fixing = false;
  }
};

/// AI の修正案をエディタに挿入して提案表示を閉じる (実行はしない)。
/// 挿入できなかった場合 (接続・ファイル未選択・接続の切替) は提案を残す。
const applyFixSuggestion = (tabId: number) => {
  const tab = resultTabs.find((t) => t.id === tabId);
  if (!tab?.fixSuggestion) {
    return;
  }
  // 結果タブは接続をまたいで残るため、提案表示中に接続を切り替えると
  // 別接続 (別方言) のファイルに挿入されてしまう。誤挿入を防ぐ
  if (selectedConnection !== tab.connection) {
    toast.warning(
      `This suggestion is for '${tab.connection}'. Switch back to that connection to apply it.`,
    );
    return;
  }
  if (insertSqlSnippet(tab.fixSuggestion)) {
    toast.success("Fixed SQL inserted into the editor");
    tab.fixSuggestion = null;
  }
};

/// AI の修正案を破棄して提案表示を閉じる。
const dismissFixSuggestion = (tabId: number) => {
  const tab = resultTabs.find((t) => t.id === tabId);
  if (tab) {
    tab.fixSuggestion = null;
  }
};

/// セル編集を適用中 (run_statements 実行中) の接続。クエリ実行と同様に
/// 同一接続の並列実行を抑止するため isConnectionRunning に含める。
let applyingConnections = $state(new Set<string>());

/// 指定した接続でクエリ実行中のタブがあるかを返す。
/// バックエンドのキャンセルレジストリは接続単位で最後の実行しか
/// 管理しないため、同一接続の並列実行はフロント側で抑止する
/// (許すとキャンセル対象の取り違えや取りこぼしが起きる)。
/// セル編集の適用中 (applyingConnections) も実行中として扱う。
const isConnectionRunning = (connection: string): boolean =>
  resultTabs.some((t) => t.running && t.connection === connection) ||
  applyingConnections.has(connection) ||
  // AI チャットのツール実行もこの接続のプールを使う。実行中に切られると
  // 途中のコネクションが壊れ、次のツール往復が追跡されないトンネルを
  // 開き直してしまう
  chatRunningConnections.has(connection);

/// 実行結果の書き込み先タブを決める。
/// アクティブな非ピン留めタブがあれば使い回し、無ければ新規タブを作る。
/// 上限到達時は最も古い非ピン留めタブを破棄する。
/// 全タブがピン留めで空きを作れない場合は null を返す (toast で通知済み)。
const prepareTargetTab = (): ResultTab | null => {
  const current = resultTabs.find((t) => t.id === activeTabId);
  if (current && !current.pinned) {
    // 同じタブへの二重書き込みを防ぐ (Cmd+Enter 連打対策)
    // タブ管理系の通知は結果ペインを覆わないよう toast で出す
    if (current.running) {
      toast.warning("A query is already running in this tab.");
      return null;
    }
    return current;
  }
  if (resultTabs.length >= MAX_RESULT_TABS) {
    const oldest = resultTabs
      .filter((t) => !t.pinned)
      .reduce<ResultTab | null>(
        (acc, t) => (acc === null || t.executedAt < acc.executedAt ? t : acc),
        null,
      );
    if (!oldest) {
      toast.warning(
        "All result tabs are pinned. Unpin or close a tab to run a new query.",
      );
      return null;
    }
    resultTabs = resultTabs.filter((t) => t.id !== oldest.id);
  }
  const tab: ResultTab = {
    id: nextTabId++,
    pinned: false,
    sql: "",
    connection: "",
    schema: null,
    executedAt: Date.now(),
    result: null,
    error: null,
    cancelled: false,
    running: false,
    fixing: false,
    fixSuggestion: null,
  };
  resultTabs = [...resultTabs, tab];
  // 生のオブジェクトではなく $state プロキシ経由の参照を返す
  // (生の参照を書き換えてもリアクティブに反映されないため)
  return resultTabs[resultTabs.length - 1];
};

/// タブに記録された接続・SQL でクエリを実行し、結果をタブへ書き込む
const executeTab = async (tab: ResultTab) => {
  tab.running = true;
  // 失敗時に前回の結果を誤認・誤エクスポートしないよう、実行前にクリアする
  tab.result = null;
  tab.error = null;
  tab.cancelled = false;
  // 前回エラーへの修正案は再実行で古くなるため破棄する
  tab.fixSuggestion = null;
  tab.executedAt = Date.now();
  activeTabId = tab.id;
  let result: QueryResult | null = null;
  let error: string | null = null;
  let cancelled = false;
  try {
    result = await api.runQuery(
      tab.connection,
      tab.sql,
      undefined,
      effectiveWritable(tab.connection),
    );
  } catch (e) {
    const message = toErrorMessage(e);
    // キャンセルによる中断はエラーではなく「Query cancelled」として表示する
    if (message === api.CANCELLED_ERROR_MESSAGE) {
      cancelled = true;
    } else {
      error = message;
    }
  }
  tab.running = false;
  // 実行中に設定再読込などでタブが破棄されていた場合は、
  // 存在しないタブ (detached なオブジェクト) へ書き込まず結果を捨てる
  if (!resultTabs.some((t) => t.id === tab.id)) {
    return;
  }
  tab.result = result;
  tab.error = error;
  tab.cancelled = cancelled;
  // \c でアクティブスキーマが切り替わっていたら表示を追従させる
  // (切替自体はバックエンドで完了済み)
  if (result?.switched_schema) {
    // 確認クエリは切替後の database で実行されているので、
    // タブが記録するスキーマもそちらに合わせる
    tab.schema = result.switched_schema;
    applySwitchedSchema(tab.connection, result.switched_schema);
  }
  // 実行中クエリの間はトンネルを切れないため、エディタタブを全て閉じた後に
  // クエリだけ走り続けていた場合は、完了したこのタイミングで切断を再判定する。
  maybeDisconnectIfIdle(tab.connection);
};

/// `\c` によるスキーマ切替をフロントの状態へ反映する。
/// スキーマブラウザは activeSchema の変化を購読しているので自動で追従し、
/// SQL 補完のスキーママップはここで取り直す。
const applySwitchedSchema = (connection: string, schema: string) => {
  // 実行中に別接続へ移っていたら、そのスキーマ表示を新接続に適用しない
  if (selectedConnection !== connection || activeSchema === schema) {
    return;
  }
  activeSchema = schema;
  // changeActiveSchema と同じ理由で AI チャットの会話は破棄する。
  // ただし `\c` はクエリの実行そのものが切替なので、切替の前に中断する
  // ことはできない (実行後に結果として知る)。中断要求が届くまでの短い間、
  // 応答待ちのエージェントが切替後のスキーマを読みうる
  clearChat();
  // 補完候補は切替先のものを取り直す (待たない)
  void loadSchemaMap();
  toast.success(`Switched to ${schema}`);
};

/// タブで実行中のクエリのキャンセルを要求する。
/// 実際の中断はバックエンドが行い、実行中の runQuery が
/// 「Query cancelled」で返ることで executeTab 側がタブに反映する。
const cancelQuery = async (id: number) => {
  const tab = resultTabs.find((t) => t.id === id);
  if (!tab || !tab.running) {
    return;
  }
  try {
    const requested = await api.cancelQuery(tab.connection);
    // 実行が直前に完了していた等でキャンセル対象が無かった場合の通知
    if (!requested) {
      toast.info("No running query to cancel. It may have just finished.");
    }
  } catch (e) {
    toast.error("Failed to cancel the query", {
      description: toErrorMessage(e),
    });
  }
};

/// 危険な文の実行確認を求め、ユーザーの応答 (true=実行) を待つ。
/// 直前の未応答の確認が残っていれば却下してから差し替える
const requestDangerousConfirm = (reason: string): Promise<boolean> =>
  new Promise((resolve) => {
    if (dangerousConfirm) {
      dangerousConfirm.resolve(false);
    }
    dangerousConfirm = { reason, resolve };
  });

/// 確認ダイアログの応答 (モーダルから呼ぶ)。ok=true で実行を続行する
const resolveDangerousConfirm = (ok: boolean) => {
  if (!dangerousConfirm) {
    return;
  }
  const { resolve } = dangerousConfirm;
  dangerousConfirm = null;
  resolve(ok);
};

/// allow_dangerous_statements が有効な接続で、危険な文なら実行前に確認を出す。
/// 実行してよければ true、キャンセルなら false を返す。
/// 無効な接続では常に true を返し (バックエンドの run_query が拒否する)、
/// 危険判定の呼び出し失敗時も true を返して実行に委ねる (allow が意図のため)。
const confirmIfDangerous = async (
  connection: string,
  sql: string,
): Promise<boolean> => {
  const info = connections.find((c) => c.name === connection);
  // 読み取り専用が効いている間 (config readonly、またはこの接続に対する実効
  // Writable が OFF) は、書き込み系の文をバックエンドが Read-only として拒否する。
  // 破壊的操作の確認は実際に実行され得る文にだけ意味があるため、ここでは確認を
  // 出さず実行へ委ねる (バックエンドが明快な Read-only エラーを返す)。実効 Writable
  // を使うので、別接続のタブ再実行 (常に読み取り専用扱い) でも無駄な確認を出さない。
  if (info?.readonly || !effectiveWritable(connection)) {
    return true;
  }
  if (!info?.allow_dangerous_statements) {
    return true;
  }
  let reason: string | null = null;
  try {
    reason = await api.checkDangerousStatement(connection, sql);
  } catch (e) {
    // 判定に失敗しても実行に委ねる (allow が意図)。ただし本当のバグを
    // 握り潰さないよう、失敗自体はコンソールに残す
    console.warn("checkDangerousStatement failed; running without confirm", e);
    return true;
  }
  if (!reason) {
    return true;
  }
  return await requestDangerousConfirm(reason);
};

/// Copy / Export で全件を取り直す時の行数上限。
/// 設定の default_limit は無視するが、無制限に読むとメモリを使い切るため
/// クライアント側の安全網は残す。打ち切った場合は truncated が立つ。
const EXPORT_MAX_ROWS = 1_000_000;

/// Copy / Export 用に、アクティブタブの SQL を default_limit 抜きで実行し直す。
///
/// 取り直すのは、表示中の結果が絞られている場合だけ:
/// - `applied_limit` が付いている (LIMIT 無しの SELECT に default_limit を付与した)
/// - `truncated` が立っている (SQL 自身の LIMIT が大きく、表示用の行数上限で切った)
///
/// **書き込みを伴う文は再実行しない。** 判定はバックエンドの
/// `can_rerun_for_output` (AI エージェント経路と同じ厳しい読み取り専用判定) に任せる。
/// `truncated` は INSERT ... RETURNING のような書き込み文でも立ちうるため、
/// applied_limit の有無だけでは安全性を担保できない。
///
/// 取り直しが不要・できない場合は null を返し、呼び出し側が表示中の結果を使う
/// (その場合の打ち切りは呼び出し側がトーストで警告する)。
const fetchResultWithoutDefaultLimit = async (
  tab: ResultTab,
): Promise<QueryResult | null> => {
  const result = tab.result;
  if (!result || (result.applied_limit == null && !result.truncated)) {
    return null;
  }
  if (!(await api.canRerunForOutput(tab.connection, tab.sql))) {
    return null;
  }
  return await api.runQuery(
    tab.connection,
    tab.sql,
    EXPORT_MAX_ROWS,
    // 元の実行と同じ権限で実行する (読み取り専用と判定済みなので書き込みは起きない)
    effectiveWritable(tab.connection),
    false,
  );
};

const runQuery = async (sql: string) => {
  // 実行先の接続を await 前に固定する。以降の await (保存・危険文の確認モーダル) の
  // 間に接続が切り替わっても、確認した接続と実行する接続が食い違わないようにする
  // (旧 SQL を新 DB で実行してしまう事故を防ぐ)。
  const connection = selectedConnection;
  // 実行前ガードの通知は、既存の結果タブを覆わないよう toast で出す
  if (!connection) {
    toast.warning("Select a connection first");
    return;
  }
  if (!sql.trim()) {
    toast.warning("There is no SQL statement to run");
    return;
  }
  // 同一接続の並列実行を抑止する (別タブで実行中でも拒否)
  if (isConnectionRunning(connection)) {
    toast.warning(
      "A query is already running on this connection. Cancel it or wait for it to finish.",
    );
    return;
  }
  // 未保存タブは best-effort で保存 (失敗しても実行は止めない。SQL はメモリ上の値)
  await flushPendingSave();
  // 危険な文 (WHERE 無し UPDATE/DELETE、DROP/TRUNCATE) は、実行を許可した
  // 接続でも実行前に確認する。キャンセルされたら何もしない
  if (!(await confirmIfDangerous(connection, sql))) {
    return;
  }
  // 確認モーダルの間に接続が切り替わっていたら、別 DB で実行しないよう中止する
  if (selectedConnection !== connection) {
    return;
  }
  errorMessage = null;
  const tab = prepareTargetTab();
  if (!tab) {
    return;
  }
  tab.sql = sql;
  tab.connection = connection;
  tab.schema = activeSchema;
  await executeTab(tab);
};

/// カーソル位置の文にエンジン別の EXPLAIN プレフィックスを付けて実行する。
/// プレフィックスの組み立てと対象判定 (SELECT / WITH のみ) はバックエンドの
/// build_explain_sql が行い、対象外の文は toast で断る。
const explainQuery = async (sql: string) => {
  if (!selectedConnection) {
    toast.warning("Select a connection first");
    return;
  }
  if (!sql.trim()) {
    toast.warning("There is no SQL statement to explain");
    return;
  }
  // 変数名は module 直下の explainSql アクションと被らないよう別名にする
  let explainStatement: string;
  try {
    explainStatement = await api.buildExplainSql(selectedConnection, sql);
  } catch (e) {
    // 対象外の文 (DML 等) や不明エンジン。実行前の断りなので warning にする
    toast.warning(toErrorMessage(e));
    return;
  }
  await runQuery(explainStatement);
};

/// EXPLAIN 結果を AI に渡すテキストに整形する (ヘッダ + タブ区切り行)。
/// 渡すのは実行計画テキストのみ (EXPLAIN 出力なので結果データではない)
const formatPlanText = (result: QueryResult): string => {
  const cellText = (value: unknown): string =>
    value === null || value === undefined
      ? "NULL"
      : typeof value === "object"
        ? JSON.stringify(value)
        : String(value);
  const lines = result.rows.map((row) => row.map(cellText).join("\t"));
  return [result.columns.join("\t"), ...lines].join("\n");
};

/// EXPLAIN 結果のタブを AI に解説させ、Markdown をモーダルに表示する
const analyzeExplainTab = async (id: number) => {
  const tab = resultTabs.find((t) => t.id === id);
  if (!tab || !tab.result || aiAnalyzing) {
    return;
  }
  aiAnalyzing = true;
  try {
    const text = await api.aiExplainPlan(
      tab.connection,
      tab.sql,
      formatPlanText(tab.result),
    );
    if (!text.trim()) {
      toast.warning("The AI returned an empty response");
      return;
    }
    aiAnalysis = text;
  } catch (e) {
    toast.error("Failed to analyze the execution plan", {
      description: toErrorMessage(e),
    });
  } finally {
    aiAnalyzing = false;
  }
};

/// AI 解説モーダルを閉じる
const closeAiAnalysis = () => {
  aiAnalysis = null;
};

/// カーソル位置の SQL 文を AI に平易に解説させ、Markdown をモーダルに
/// 表示する (実行はしない)。LLM に送るのは SQL とスキーマ情報のみで、
/// クエリの結果データは送らない (バックエンドの ai_explain_sql 参照)
const explainSql = async (sql: string) => {
  if (!selectedConnection) {
    toast.warning("Select a connection first");
    return;
  }
  if (!sql.trim()) {
    toast.warning("There is no SQL statement to explain");
    return;
  }
  // 二重実行防止 (ボタンも disabled にしているが防御的にガードする)
  if (aiExplaining) {
    return;
  }
  aiExplaining = true;
  try {
    const text = await api.aiExplainSql(selectedConnection, sql);
    if (!text.trim()) {
      toast.warning("The AI returned an empty response");
      return;
    }
    aiExplanation = text;
  } catch (e) {
    toast.error("Failed to explain the SQL statement", {
      description: toErrorMessage(e),
    });
  } finally {
    aiExplaining = false;
  }
};

/// AI による選択 SQL 解説のモーダルを閉じる
const closeAiExplanation = () => {
  aiExplanation = null;
};

/// チャットメッセージの ID 採番 (表示用の key。バックエンドへは送らない)
let nextChatMessageId = 1;

/// チャットの世代。会話を破棄するたびに進める
/// (接続切替・スキーマ切替・Clear・設定リロード)。
/// 応答待ちの間に会話が破棄されたかを、接続名の比較だけでなくこの世代でも見る:
/// 設定リロードは同じ名前の接続を作り直すため、名前だけでは「リロードを挟んだ
/// 古い接続の応答」を弾けない (バックエンドはリロード前の接続設定・スキーマの
/// まま応答を返す)。スキーマ切替も接続名は変わらないため同様。
let chatGeneration = $state(0);

/// チャットの往復の開始を記録する。
const retainChatRequest = (connection: string, requestId: string) => {
  const next = new Map(chatRunningConnections);
  next.set(connection, new Set(next.get(connection)).add(requestId));
  chatRunningConnections = next;
};

/// チャットの往復の終了を記録する。
/// その接続で走っている最後の 1 本だった場合だけ true を返す
/// (切断の再判定は最後の 1 本の完了時にだけ行う)。
const releaseChatRequest = (connection: string, requestId: string): boolean => {
  const next = new Map(chatRunningConnections);
  const ids = new Set(next.get(connection));
  ids.delete(requestId);
  if (ids.size > 0) {
    next.set(connection, ids);
  } else {
    next.delete(connection);
  }
  chatRunningConnections = next;
  return ids.size === 0;
};

/// AI チャット (右ペイン) にユーザーの発言を積み、応答を 1 往復もらう。
/// 会話履歴は毎回まるごとバックエンドへ送る (会話状態はフロントが持つ)。
const sendChatMessage = async (text: string) => {
  const message = text.trim();
  if (!message) {
    return;
  }
  if (!selectedConnection) {
    toast.warning("Select a connection first");
    return;
  }
  // 二重送信防止 (送信ボタンも disabled にしているが防御的にガードする)。
  // 破棄済みの往復が残っている間は送信を許す (新しい会話を待たせない)
  if (chatSendingGen !== null && chatSendingGen === chatGeneration) {
    return;
  }
  // 接続 / スキーマ切替・設定リロードの遷移中は受け付けない
  if (chatTransitions > 0) {
    return;
  }
  const connection = selectedConnection;
  const generation = chatGeneration;
  chatMessages = [
    ...chatMessages,
    { id: nextChatMessageId++, role: "user", content: message },
  ];
  // 失敗した応答は履歴に残すが LLM へは送らない (エラー文言を会話の一部と
  // 誤解させないため)
  const history: ChatTurn[] = chatMessages
    .filter((m) => !m.failed)
    .map((m) => ({ role: m.role, content: m.content }));
  chatSendingGen = generation;
  // 中断はこの ID を指定して行う (同じ接続で複数の往復が走りうる)
  const requestId = `chat-${nextChatRequestSeq++}`;
  // エージェントのツール実行中はこの接続を「実行中」として扱い、
  // エディタタブが無いだけでトンネル / プールを切られないようにする
  retainChatRequest(connection, requestId);
  try {
    const reply = await api.aiChat(connection, history, requestId);
    // 応答待ちの間に会話が破棄されていたら捨てる (接続切替・Clear・設定
    // リロード)。設定リロードは同名の接続を作り直すため、接続名の比較だけ
    // では弾けない
    if (selectedConnection !== connection || chatGeneration !== generation) {
      return;
    }
    // 失敗した往復も (reject ではなく) error 付きで返る。実行した
    // クエリは成功時と同じく表示する
    chatMessages = [
      ...chatMessages,
      {
        id: nextChatMessageId++,
        role: "assistant",
        content: reply.error ?? reply.content,
        toolCalls: reply.tool_calls,
        failed: reply.error !== null,
      },
    ];
  } catch (e) {
    if (selectedConnection !== connection || chatGeneration !== generation) {
      return;
    }
    chatMessages = [
      ...chatMessages,
      {
        id: nextChatMessageId++,
        role: "assistant",
        content: toErrorMessage(e),
        failed: true,
      },
    ];
  } finally {
    // より新しい往復が始まっていたら、その待機状態を消さない
    if (chatSendingGen === generation) {
      chatSendingGen = null;
    }
    // クエリ実行と同じく、実行が終わった時点で切断を再判定する
    // (実行中は切れないので、他の接続へ移った後もトンネルが残っている)。
    // 同じ接続で別の往復がまだ走っているなら判定しない
    if (releaseChatRequest(connection, requestId)) {
      maybeDisconnectIfIdle(connection);
    }
  }
};

/// 走っているエージェントに中断を要求し、要求がバックエンドに届くまで待つ。
/// 中断要求の失敗で呼び出し側の処理を止めない (実行はいずれ終わる)。
const requestChatCancel = async () => {
  await Promise.all(
    [...chatRunningConnections.entries()].map(([connection, ids]) =>
      api.cancelAiChat(connection, [...ids]).catch(() => {}),
    ),
  );
};

/// 応答待ちのエージェントを止める (Stop ボタン)。会話は残すので、
/// 中断されたことは失敗メッセージとして会話に出る。
const stopChat = () => {
  void requestChatCancel();
};

/// AI チャットの会話を捨てる
/// (Clear ボタン / 接続切替 / スキーマ切替 / 設定リロード)。
/// 世代を進めて、応答待ちの往復が破棄後の会話へ書き込むのを防ぐ。
/// 待機表示 (chatSending) も世代で判定するため、破棄と同時に消える。
///
/// 応答を捨てるだけではバックエンドのエージェントは走り続け、重いクエリや
/// 次のツール往復 (切替後のスキーマを読む) がそのまま実行されてしまうため、
/// 実行中の接続には中断も要求する。
const clearChat = () => {
  chatGeneration++;
  chatMessages = [];
  void requestChatCancel();
};

/// 会話を破棄し、中断要求がバックエンドに届くまで待つ。
/// バックエンドの状態を書き換える操作 (スキーマ切替・設定リロード) の
/// **前**に使う: 投げっぱなしだと、中断カウンタが進む前に切替が先行し、
/// 古いエージェントが新しいスキーマ / プールでツール実行を続けうる。
const clearChatAndWait = async () => {
  chatGeneration++;
  chatMessages = [];
  // 中断の到達を待つ間に新しい往復を始めさせない (始まってしまうと、
  // その往復は中断対象に入らないまま切替後のプールを使う)。
  // 呼び出し側は切替の完了後に endChatTransition() を呼ぶ
  chatTransitions++;
  await requestChatCancel();
};

/// clearChatAndWait で始めた遷移の終了 (成否によらず必ず 1 回だけ呼ぶ)。
const endChatTransition = () => {
  chatTransitions = Math.max(0, chatTransitions - 1);
};

/// タブに記録された SQL を同じ接続で再実行する
const rerunTab = async (id: number) => {
  const tab = resultTabs.find((t) => t.id === id);
  if (!tab || tab.running || !tab.sql.trim()) {
    return;
  }
  // 同一接続の並列実行を抑止する (別タブで実行中でも拒否)
  if (isConnectionRunning(tab.connection)) {
    toast.warning(
      "A query is already running on this connection. Cancel it or wait for it to finish.",
    );
    return;
  }
  // 再実行でも危険な文は確認する (通常実行と同じガード)
  if (!(await confirmIfDangerous(tab.connection, tab.sql))) {
    return;
  }
  errorMessage = null;
  // 接続のアクティブスキーマは実行時点から変わっている可能性があるため、
  // 表示が実際の実行先とずれないよう再取得する (失敗しても実行は続ける)
  try {
    tab.schema = (await api.getActiveSchema(tab.connection)) ?? tab.schema;
  } catch {
    // 取得失敗時は記録済みのスキーマ表示を維持する
  }
  await executeTab(tab);
};

/// 結果グリッドのセル編集 (UPDATE 群) を 1 トランザクションで適用する。
/// 適用は「アクティブ接続かつ Writable ON」の時だけ (UI でも抑止しているが、
/// 別接続タブの誤適用や無駄な往復を防ぐためここでもガードする)。成功したら
/// 表示値を実際の DB 状態に合わせるため再実行し、true を返す。
const submitCellEdits = async (
  tabId: number,
  statements: string[],
): Promise<boolean> => {
  const tab = resultTabs.find((t) => t.id === tabId);
  if (!tab || statements.length === 0) {
    return false;
  }
  if (!effectiveWritable(tab.connection)) {
    toast.warning(
      "Turn on the Writable switch for this connection to apply edits.",
    );
    return false;
  }
  // 生成 UPDATE はスキーマ未修飾で「接続の現在のアクティブスキーマ」に走るため、
  // 編集後にスキーマを切り替えていると別スキーマの同名テーブルを更新し得る。
  // タブの実行時スキーマと現在のアクティブスキーマが違えば適用しない
  // (UI の canEditActiveConnection は新規編集のみ抑止するので、Submit 側でも防ぐ)。
  if (tab.schema !== activeSchema) {
    toast.warning(
      "The active schema changed since these edits were made. Cancel them and re-run the query.",
    );
    return false;
  }
  // 同一接続で実行中 (クエリまたは別の適用) なら適用しない
  // (キャンセル対象の取り違え防止・Submit 連打防止。runQuery と同じ不変条件)
  if (isConnectionRunning(tab.connection)) {
    toast.warning(
      "A query is already running on this connection. Wait for it to finish.",
    );
    return false;
  }
  const connection = tab.connection;
  // 適用中は接続を「実行中」に登録し、並列実行・二重 Submit を抑止する
  applyingConnections = new Set(applyingConnections).add(connection);
  let affected: number;
  try {
    affected = await api.runStatements(
      connection,
      statements,
      effectiveWritable(connection),
    );
  } catch (e) {
    toast.error("Failed to apply the changes", {
      description: toErrorMessage(e),
    });
    return false;
  } finally {
    // rerunTab は isConnectionRunning を見て早期 return するため、再取得の前に外す
    const next = new Set(applyingConnections);
    next.delete(connection);
    applyingConnections = next;
  }
  toast.success(`Applied ${affected} row change${affected === 1 ? "" : "s"}`);
  // 表示を DB の実際の値に合わせるため再取得する (適用自体は成功済み)
  await rerunTab(tabId);
  return true;
};

const selectResultTab = (id: number) => {
  if (resultTabs.some((t) => t.id === id)) {
    activeTabId = id;
  }
};

const closeResultTab = (id: number) => {
  const index = resultTabs.findIndex((t) => t.id === id);
  if (index < 0) {
    return;
  }
  // 実行中のタブを閉じると in-flight のクエリ状態が UI から消えてしまうため拒否する
  // (閉じるボタンも disabled にしているが、防御的にここでもガードする)
  if (resultTabs[index].running) {
    toast.warning("Cannot close a tab while its query is running.");
    return;
  }
  resultTabs = resultTabs.filter((t) => t.id !== id);
  if (activeTabId === id) {
    // 閉じたタブの右隣 (無ければ左隣) をアクティブにする
    const neighbor = resultTabs[index] ?? resultTabs[index - 1] ?? null;
    activeTabId = neighbor?.id ?? null;
  }
};

const toggleResultTabPin = (id: number) => {
  const tab = resultTabs.find((t) => t.id === id);
  if (tab) {
    tab.pinned = !tab.pinned;
  }
};

/// ===== 外部ファイル変更のウォッチャ =====
/// 開いているクエリファイルがアプリ外 (別エディタ・git・他プロセス) で変更された時に、
/// 未編集なら自動リロードし、編集中なら 3-way マージを試み、衝突時は警告する。
/// クエリファイルの読込はローカル FS のみ (DB / SSH トンネルには触れない) ため、
/// バックグラウンドで定期的に読み直しても接続を開いてしまうことはない。

let fileWatchTimer: ReturnType<typeof setInterval> | null = null;
/// tick の多重実行を防ぐ (ファイル読込は非同期のため)。
let fileWatchTicking = false;
/// タブごとに「既に衝突を警告したディスク内容」を覚え、同じ状態で毎 tick 警告を
/// 繰り返さないようにする。解消・タブクローズでエントリを消す。
const conflictNotified = new Map<number, string>();

/// このタブの保留中の自動保存予約を解除する (外部変更を取り込んだ後に、古い
/// エディタ内容で上書き保存し直されるのを防ぐ)。
const cancelPendingSaveFor = (tabId: number) => {
  if (autoSavePendingTabId === tabId) {
    if (autoSaveTimer) {
      clearTimeout(autoSaveTimer);
      autoSaveTimer = null;
    }
    autoSavePendingTabId = null;
  }
};

/// 1 つのエディタタブについて外部変更を調べて反映する。
const checkTabForExternalChange = async (tabId: number) => {
  const before = editorTabs.find((t) => t.id === tabId);
  if (!before) {
    return;
  }
  const { connection, file } = before;
  // 読込前の base を控える。読込中に自動保存が完了して diskContent が進むと、
  // 古い read 結果を新しい base への外部変更と誤認しかねないため、読込後に
  // base が変わっていたらこの tick は見送る (次の tick で最新状態を見る)。
  const baseAtStart = before.diskContent;
  let disk: string;
  try {
    disk = await api.readQueryFile(connection, file);
  } catch {
    // 読めない (外部で削除された等) 場合は何もしない。次の契機に委ねる。
    return;
  }
  // await の間にタブが閉じられた / ファイル名が変わった可能性があるので取り直す。
  const tab = editorTabs.find((t) => t.id === tabId);
  if (!tab || tab.connection !== connection || tab.file !== file) {
    return;
  }
  // 読込中に保存が回り込んで base (diskContent) が変わっていたら、この read は
  // 古い可能性があるので判定しない。
  if (tab.diskContent !== baseAtStart) {
    return;
  }

  const base = tab.diskContent;
  if (disk === base) {
    // 外部変更なし。過去の衝突警告が残っていればクリアする。
    conflictNotified.delete(tabId);
    // 衝突を起こした外部変更が元に戻り disk が base と一致した場合、衝突は解消。
    // conflicted を残すと、外部変更が無いのに dirty タブが衝突扱いのままとなり、
    // 閉じる/リロード時に手元の編集が破棄されてしまう。フラグを下ろし、まだ dirty
    // なら通常のデバウンス自動保存を張り直して編集を確実に永続化する
    // (衝突検知時に cancelPendingSaveFor で予約を外しているため、張り直さないと
    //  次の編集まで自動保存されない)。
    if (tab.conflicted) {
      tab.conflicted = false;
      if (tab.dirty) {
        scheduleAutoSave(tabId);
      }
    }
    return;
  }
  // ここに来た = ディスクが最後に把握した内容と異なる (外部変更あり)。
  if (tab.content === base) {
    // 手元に未保存の編集が無い → 外部の内容をそのまま採用してリロードする。
    tab.content = disk;
    tab.diskContent = disk;
    tab.dirty = false;
    tab.conflicted = false;
    cancelPendingSaveFor(tabId);
    conflictNotified.delete(tabId);
    toast.info(`Reloaded "${file}" (changed on disk)`);
    return;
  }
  if (tab.content === disk) {
    // 手元の編集が結果的にディスクと一致 (保存が回り込んだ等)。表示は変えず基準だけ更新。
    tab.diskContent = disk;
    tab.dirty = false;
    tab.conflicted = false;
    conflictNotified.delete(tabId);
    return;
  }
  // base / local / remote が三者三様 → 3-way マージを試みる。
  // マージに使った手元の内容を控える (書き込み await 中にユーザーが更に打鍵した場合、
  // その新しい編集を merged で黙って上書きしないため)。
  const localSnapshot = tab.content;
  const { merged, conflict } = merge3(base, localSnapshot, disk);
  if (!conflict) {
    // 別々の箇所への変更なので自動マージできる。結果をディスクへ書き戻す。
    // ただし force で無条件に書くと、この tick の read (disk) 〜 書き込み完了の間に
    // さらに別プロセスが保存した場合、古い disk を基にした merged で新しい外部保存を
    // 黙って上書きしてしまう。マージの基にした disk スナップショットとディスクが
    // まだ一致する時だけ書く CAS にする (expectedBase=disk)。食い違っていたら書かず、
    // 次 tick で最新内容に対して改めてマージ/衝突判定させる。
    let wrote = false;
    try {
      wrote = await api.writeQueryFileIfUnchanged(connection, file, merged, disk);
    } catch (e) {
      errorMessage = `Failed to save the file: ${toErrorMessage(e)}`;
    }
    // 書き込み await 中にタブが閉じられた / 別ファイルになった / base が進んだ場合は
    // この結果を反映しない (次 tick で最新状態を見る)。
    const t2 = editorTabs.find((t) => t.id === tabId);
    if (
      !t2 ||
      t2.connection !== connection ||
      t2.file !== file ||
      t2.diskContent !== base
    ) {
      return;
    }
    if (wrote) {
      // マージ結果をディスクへ反映できた。ディスクは今 merged。
      t2.diskContent = merged;
      t2.conflicted = false;
      conflictNotified.delete(tabId);
      if (t2.content === localSnapshot) {
        // 書き込み中に追加編集は無い → エディタもマージ結果に確定する。
        t2.content = merged;
        t2.dirty = false;
        cancelPendingSaveFor(tabId);
        toast.info(`Merged external changes into "${file}"`);
      } else {
        // await 中にさらに打鍵された → その新しい編集は捨てず dirty のまま残す。
        // 基準 (diskContent) は merged に進めたので、次 tick / 予約済み自動保存が
        // 新編集を merged を base として保存/マージする (予約はそのまま活かす)。
      }
    } else {
      // マージ中にさらに外部書き込みが入った。今回は反映せず (手元の編集はそのまま)、
      // dedup キーを消して次 tick で最新ディスク内容に対して再判定させる。
      conflictNotified.delete(tabId);
    }
    return;
  }
  // 同じ箇所を双方が別々に変更 → 自動マージ不可。未保存の編集は保持し、警告する。
  // 保留中の自動保存は解除する (残すと衝突中のタブに対して debounce 保存が走りかねない)。
  cancelPendingSaveFor(tabId);
  // 衝突中は自動保存 (閉じる・アプリ終了・reloadConnections) を抑止し、手元の編集で
  // 外部変更を黙って上書きするのを防ぐ。解消はユーザーの明示操作 (ツールバーの
  // Overwrite で上書き / Discard で破棄) に委ねる。なお暗黙の自動保存自体も
  // saveEditorTab の CAS で外部変更を上書きしないよう二重に守られている。
  tab.conflicted = true;
  // 同じディスク状態で毎 tick 警告しないよう既通知を記録する。
  if (conflictNotified.get(tabId) !== disk) {
    conflictNotified.set(tabId, disk);
    toast.warning(`"${file}" was changed on disk, but you have unsaved edits`, {
      description:
        "Your edits are kept. Use Overwrite or Discard in the editor toolbar to resolve.",
    });
  }
};

const fileWatchTick = async () => {
  if (fileWatchTicking) {
    return;
  }
  fileWatchTicking = true;
  try {
    // 開始時点の id スナップショットで回す (処理中に配列が変わっても安全)。
    const ids = editorTabs.map((t) => t.id);
    for (const id of ids) {
      await checkTabForExternalChange(id);
    }
    // 閉じられたタブの通知記録を掃除する。
    const alive = new Set(editorTabs.map((t) => t.id));
    for (const id of [...conflictNotified.keys()]) {
      if (!alive.has(id)) {
        conflictNotified.delete(id);
      }
    }
  } finally {
    fileWatchTicking = false;
  }
};

/// 外部ファイル変更のポーリングを開始する (+page の onMount から呼ぶ)。
const startFileWatcher = () => {
  if (fileWatchTimer) {
    return;
  }
  fileWatchTimer = setInterval(
    () => void fileWatchTick(),
    FILE_WATCH_INTERVAL_MS,
  );
};

/// ポーリングを停止する (+page の onDestroy から呼ぶ)。
const stopFileWatcher = () => {
  if (fileWatchTimer) {
    clearInterval(fileWatchTimer);
    fileWatchTimer = null;
  }
  conflictNotified.clear();
};

export default {
  get connections() {
    return connections;
  },
  get selectedConnection() {
    return selectedConnection;
  },
  /// Writable スイッチの状態。false の間は書き込み系の文を実行できない
  get writable() {
    return writable;
  },
  /// 選択中の接続が config で readonly: true か (スイッチでは解除できない)。
  /// 接続未選択なら false。トグルのロック表示に使う
  get selectedConnectionReadonly() {
    return (
      connections.find((c) => c.name === selectedConnection)?.readonly ?? false
    );
  },
  /// Writable スイッチを切り替える。config readonly 接続では書き込みは
  /// バックエンドが拒否するが、状態自体は接続横断のセッション設定として保持する
  toggleWritable() {
    writable = !writable;
  },
  /// 選択中の接続のエンジン能力宣言 (未選択なら null)。
  /// TABLES ペインの表示可否やクエリファイル拡張子の決定に使う
  get selectedCapabilities() {
    return (
      connections.find((c) => c.name === selectedConnection)?.capabilities ??
      null
    );
  },
  /// 選択中の接続のクエリファイル拡張子 (ドット無し。未選択なら "sql")
  get selectedFileExtension() {
    return this.selectedCapabilities?.file_extension ?? "sql";
  },
  get files() {
    return files;
  },
  /// 開いているエディタタブ (全接続横断・多段表示)
  get editorTabs() {
    return editorTabs;
  },
  get activeEditorTabId() {
    return activeEditorTabId;
  },
  /// アクティブなエディタタブのファイル名 (無ければ null)
  get selectedFile() {
    return getActiveEditorTab()?.file ?? null;
  },
  /// アクティブなエディタタブの内容 (無ければ空文字)
  get editorContent() {
    return getActiveEditorTab()?.content ?? "";
  },
  get resultTabs() {
    return resultTabs;
  },
  get activeTabId() {
    return activeTabId;
  },
  /// アクティブな結果タブ (無ければ null)
  get activeResultTab() {
    return resultTabs.find((t) => t.id === activeTabId) ?? null;
  },
  get errorMessage() {
    return errorMessage;
  },
  /// いずれかのタブでクエリ実行中なら true (Run ボタンの無効化などに使う)
  get running() {
    return resultTabs.some((t) => t.running);
  },
  get loadingConnections() {
    return loadingConnections;
  },
  /// アクティブなエディタタブに未保存の編集があるか
  get dirty() {
    return getActiveEditorTab()?.dirty ?? false;
  },
  /// アクティブなエディタタブが外部変更との衝突状態か (ツールバーの解消 UI 表示に使う)
  get activeFileConflicted() {
    return getActiveEditorTab()?.conflicted ?? false;
  },
  get schemas() {
    return schemas;
  },
  get activeSchema() {
    return activeSchema;
  },
  get aiInfo() {
    return aiInfo;
  },
  get aiError() {
    return aiError;
  },
  get aiGenerating() {
    return aiGenerating;
  },
  get aiAnalyzing() {
    return aiAnalyzing;
  },
  /// AI による実行計画解説の Markdown (モーダル表示中のみ非 null)
  get aiAnalysis() {
    return aiAnalysis;
  },
  get aiExplaining() {
    return aiExplaining;
  },
  /// AI による選択 SQL 解説の Markdown (モーダル表示中のみ非 null)
  get aiExplanation() {
    return aiExplanation;
  },
  /// AI チャット (右ペイン) の表示中メッセージ
  get chatMessages() {
    return chatMessages;
  },
  /// AI チャットの応答待ち (破棄済みの往復は待機扱いにしない)。
  /// 切替の遷移中も入力を塞ぐため true にする
  get chatSending() {
    return (
      chatTransitions > 0 ||
      (chatSendingGen !== null && chatSendingGen === chatGeneration)
    );
  },
  /// SQL 補完用のテーブル名 → カラム名リストのマップ (未取得なら null)
  get schemaMap() {
    return schemaMap;
  },
  /// 危険な文の実行確認ダイアログに表示する理由 (非表示中は null)
  get dangerousConfirmReason() {
    return dangerousConfirm?.reason ?? null;
  },
  /// 確認ダイアログで「実行する」を選んだ
  confirmDangerous: () => resolveDangerousConfirm(true),
  /// 確認ダイアログで「キャンセル」を選んだ
  cancelDangerous: () => resolveDangerousConfirm(false),
  loadConnections,
  loadAiInfo,
  generateSql,
  loadSchemaMap,
  ensureConnectionResources,
  reloadConnections,
  selectConnection,
  changeActiveSchema,
  selectFile,
  openFileByTarget,
  activateEditorTab,
  closeEditorTab,
  createFile,
  deleteFile,
  renameFile,
  moveFileToConnection,
  copyFilePath,
  saveCurrentFile,
  discardActiveFileConflict,
  overwriteActiveFileConflict,
  updateEditorContent,
  insertSqlSnippet,
  fixSqlWithAi,
  applyFixSuggestion,
  dismissFixSuggestion,
  isConnectionRunning,
  runQuery,
  fetchResultWithoutDefaultLimit,
  explainQuery,
  analyzeExplainTab,
  closeAiAnalysis,
  explainSql,
  closeAiExplanation,
  sendChatMessage,
  clearChat,
  stopChat,
  cancelQuery,
  rerunTab,
  submitCellEdits,
  selectResultTab,
  closeResultTab,
  toggleResultTabPin,
  startFileWatcher,
  stopFileWatcher,
};
