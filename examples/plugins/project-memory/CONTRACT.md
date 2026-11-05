# P0 implementation contract

Standalone Rust binary, existing Kroma plugin API 1. No core changes, model hooks,
context backend, network, embedding, or automatic claims about reviewed code.
Private storage under supplied data_dir/workspaces/SHA256(canonical workdir)/.
Different workdirs do not share automatically. Review notes are historical
navigation aids, never proof that code is still safe or that a fix was verified.
Types and validation: src/model.rs. All metadata and note text are untrusted data.

Store interface (anyhow::Result, names exact):
- Store::open(path: &Path, scope_identity: &str) -> Store
- insert(&mut self, input: &NewMemory, accepted: bool) -> Memory
- accept(&mut self, id: &str) -> Memory
- correct(&mut self, id: &str, text: &str) -> Memory
- forget(&mut self, id: &str) -> ()
- purge(&mut self, id: &str) -> ()
- import_legacy(&mut self, notes: &[String], source_id: &str) -> ImportReport
- get(&self, id: &str) -> Memory (admin, accepted/candidate only)
- read(&self, ids: &[String]) -> Vec<Memory> (model, accepted only, max8)
- search(&self, query: &str, paths: &[String], limit: usize) -> Vec<SearchHit>
- list(&self, state: &str, limit: usize, offset: usize) -> Vec<Memory>
- settings(&self) -> Settings
- set_mode(&mut self, use_enabled: Option<bool>, capture_enabled: Option<bool>) -> Settings
- stats(&self) -> Stats

Storage split: store.rs owns schema, mutations, pub(crate) conn: Connection;
store_read.rs owns read methods as impl Store. Each memories row has columns
id TEXT PRIMARY KEY, kind TEXT, text TEXT, state TEXT, version INTEGER,
paths TEXT (JSON Vec<String>), evidence_refs TEXT (JSON Vec<String>),
source TEXT (JSON Source), created_at INTEGER, updated_at INTEGER,
fingerprint TEXT UNIQUE. These exact column names form the internal contract.
metadata(key TEXT PRIMARY KEY,value TEXT) stores identity,use_enabled,capture_enabled.
versions(memory_id,version,body) holds JSON Memory snapshots. imports(source_id PRIMARY KEY).
Schema user_version1; reject newer versions. Triggers/FTS to be added by read module
initializer if needed, not required in mutation implementation.

Every mutation uses an IMMEDIATE transaction; commit before successful reply.
FULL durability, WAL, foreign keys, busy_timeout <=1s. Bound rows10000/candidates200.
Opaque random IDs. SHA256 fingerprint(kind,trimmed text,sorted paths), JSON encoding
before hashing. Identical candidate can be explicitly promoted by manual remember;
model proposals cannot promote. Tombstones and fingerprints survive purge to
prevent exact-text resurrection/import replay. Purge wipes old versions/body/source.
No secret echo in errors; SQL parameters not concatenated user strings.

Scope interface: Scope {pub id:String,pub workdir:String,pub folder:PathBuf,
pub database:PathBuf}; resolve(data_dir:&Path, workdir:&Path)->Result<Scope>.
Private real dirs (0700 Unix/current user+SYSTEM Windows DACL), real db0600.
Reject symlinks/reparse points/sidecars and known UNC/network paths. Canonical
workdir UTF8, require absolute data_dir. May not defend hostile same-user races.
Privacy interface: check_text(&str)->Result<()>; block obvious credentials without
rejecting ordinary discussion. It is best-effort, not a guarantee of secret detection.

V1 wire: input {protocol_version:1,request_id:N,type:tool|command|ui|ping,name,
arguments,workdir,data_dir,session_id?,call_id?}; reply {protocol_version:1,
request_id:N,content:STRING,is_error:bool}. Content is JSON-string for tools,
human-readable for /memory command and ui. Ping must not create storage.
Tools project_memory_search/read/propose only; model writes ALWAYS candidate.
Command /memory: status, remember [kind] TEXT, review TEXT, search TEXT,
show ID, candidates [offset], accept ID, correct ID TEXT, forget ID,
purge ID --confirm, use on|off, capture on|off, off,
export [offset] (paged Markdown returned, no arbitrary output path),
import FILE --confirm (bounded legacy JSON string array, all validate before commit), doctor.
Replies <=128KiB, input frame <=128KiB; reject oversize, malformed and unknown
arguments, continue worker after ordinary request errors. Tools cannot set scope,
source origin,state or call privileged commands. Note references never dereferenced.
