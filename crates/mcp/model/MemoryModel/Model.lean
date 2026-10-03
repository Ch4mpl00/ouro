/-
The reference model of memory documents: what `patch_doc`, `append_doc`,
`write_doc`, `revert_patch`, `read_doc` and `doc_history` mean, with nothing
else in the way — no database, no concurrency, no indexing.

The real server (crates/mcp/src/memory.rs) is checked against this file:
tests/model.rs runs random sequences of calls through both and requires the
same answers (sequentially and under concurrency). Theorems.lean proves
properties of this model, so the model itself is checked too.

One project; documents by name. Text is a list of characters, so every rule
below is ordinary list code.
-/
namespace Memory

abbrev Text := List Char

inductive Kind
  | write | append | patch | revert
  deriving DecidableEq, BEq, Hashable, Repr

structure Edit where
  old : Text
  new : Text
  deriving DecidableEq, BEq, Hashable, Repr

/-- Every change is recorded with the body it replaced. -/
structure Patch where
  kind : Kind
  edits : List Edit
  bodyBefore : Text
  versionBefore : Nat
  versionAfter : Nat
  deriving DecidableEq, BEq, Hashable, Repr

structure Doc where
  body : Text
  version : Nat
  /-- Oldest first. Never rewritten, only extended. -/
  history : List Patch
  deriving DecidableEq, BEq, Hashable, Repr

/-- A document that does not exist yet behaves as an empty one at version 0. -/
def Doc.empty : Doc := { body := [], version := 0, history := [] }

abbrev World := List (String × Doc)

def World.find (w : World) (name : String) : Option Doc :=
  (w.find? (·.1 == name)).map (·.2)

def World.set (w : World) (name : String) (d : Doc) : World :=
  (name, d) :: w.filter (·.1 != name)

/-! ## Calls and answers -/

inductive Cmd
  /-- `expected`: none when creating; the current version when overwriting. -/
  | write (doc : String) (body : Text) (expected : Option Nat)
  | append (doc : String) (text : Text)
  | patch (doc : String) (expected : Nat) (edits : List Edit)
  /-- `index`: position in the document's history, oldest = 0. -/
  | revert (doc : String) (index : Nat) (rollback : Bool)
  | read (doc : String)
  | history (doc : String)
  deriving Repr, Inhabited

inductive Reply
  | written (version : Nat) (sizeBytes : Nat)
  | read (body : Text) (version : Nat)
  /-- Newest first: kind, version before, version after. -/
  | history (entries : List (Kind × Nat × Nat))
  | docNotFound
  /-- Overwriting an existing document without saying which version you read. -/
  | versionRequired (current : Nat)
  | versionConflict (current : Nat)
  /-- A quote matched zero times or more than once. Nothing was written. -/
  | editFailed
  | patchNotFound
  /-- Later patches touched this patch's text; a rollback would restore this version. -/
  | revertConflict (rollbackTo : Nat)
  /-- Bad parameters: rejected before memory is consulted. -/
  | rejected
  deriving DecidableEq, Repr

def Reply.isError : Reply → Bool
  | .written .. | .read .. | .history .. => false
  | _ => true

/-! ## Text rules -/

/-- How many times `old` occurs in `s`. Overlapping matches count:
"aaa" contains "aa" twice, because the quote could mean either. -/
def occurrences (old : Text) : Text → Nat
  | [] => 0
  | s@(_ :: rest) => (if old.isPrefixOf s then 1 else 0) + occurrences old rest

/-- Replace the first occurrence of `old` by `new`. -/
def replaceFirst (old new : Text) : Text → Text
  | [] => []
  | s@(c :: rest) => if old.isPrefixOf s then new ++ s.drop old.length else c :: replaceFirst old new rest

/-- All edits apply, in order, or none do. Each `old` must occur exactly once
in the text as the earlier edits left it. -/
def applyEdits (body : Text) : List Edit → Option Text
  | [] => some body
  | e :: rest => if occurrences e.old body = 1 then applyEdits (replaceFirst e.old e.new body) rest else none

/-- The edits that undo `edits`. A deletion cannot be undone in place —
there is nothing left to anchor the re-insertion to. -/
def invert (edits : List Edit) : Option (List Edit) :=
  edits.reverse.mapM fun e => if e.new = [] then none else some { old := e.new, new := e.old }

def trimEnd (t : Text) : Text := (t.reverse.dropWhile Char.isWhitespace).reverse
def trim (t : Text) : Text := trimEnd (t.dropWhile Char.isWhitespace)

/-- Appending adds a paragraph: one blank line before it, exactly one newline
at the end. -/
def joinParagraphs (body addition : Text) : Text :=
  let base := trimEnd body
  match base.isEmpty, addition.isEmpty with
  | true, true => []
  | false, true => base ++ ['\n']
  | true, false => addition ++ ['\n']
  | false, false => base ++ ['\n', '\n'] ++ addition ++ ['\n']

def sizeBytes (t : Text) : Nat := (String.ofList t).utf8ByteSize

/-! ## The operations -/

/-- Every successful write: bump the version, record what was replaced. -/
def commit (w : World) (name : String) (d : Doc) (kind : Kind) (edits : List Edit) (body : Text) : World × Reply :=
  let p : Patch := { kind, edits, bodyBefore := d.body, versionBefore := d.version, versionAfter := d.version + 1 }
  (w.set name { body, version := d.version + 1, history := d.history ++ [p] }, .written (d.version + 1) (sizeBytes body))

def write (w : World) (name : String) (body : Text) (expected : Option Nat) : World × Reply :=
  match w.find name with
  | none =>
    if expected = none ∨ expected = some 0 then commit w name .empty .write [] body
    else (w, .versionConflict 0)
  | some d =>
    match expected with
    | none => (w, .versionRequired d.version)
    | some v => if v = d.version then commit w name d .write [] body else (w, .versionConflict d.version)

def append (w : World) (name : String) (text : Text) : World × Reply :=
  if text = [] then (w, .rejected) else
  match w.find name with
  | none => (w, .docNotFound)
  | some d => commit w name d .append [] (joinParagraphs d.body (trim text))

def patch (w : World) (name : String) (expected : Nat) (edits : List Edit) : World × Reply :=
  if expected = 0 ∨ edits = [] ∨ edits.any (·.old = []) then (w, .rejected) else
  match w.find name with
  | none => (w, .docNotFound)
  | some d =>
    if expected ≠ d.version then (w, .versionConflict d.version) else
    match applyEdits d.body edits with
    | none => (w, .editFailed)
    | some body => commit w name d .patch edits body

/-- The newest patch, or any patch with `rollback`, restores the body it
replaced. An older one is undone in place by inverting its edits, which works
only while later patches left its text alone. -/
def revert (w : World) (name : String) (index : Nat) (rollback : Bool) : World × Reply :=
  match w.find name with
  | none => (w, .docNotFound)
  | some d =>
    match d.history[index]? with
    | none => (w, .patchNotFound)
    | some p =>
      if rollback ∨ index + 1 = d.history.length then commit w name d .revert [] p.bodyBefore else
      match (if p.edits = [] then none else invert p.edits) >>= applyEdits d.body with
      | none => (w, .revertConflict p.versionBefore)
      | some body => commit w name d .revert [] body

def step (w : World) : Cmd → World × Reply
  | .write name body expected => write w name body expected
  | .append name text => append w name text
  | .patch name expected edits => patch w name expected edits
  | .revert name index rollback => revert w name index rollback
  | .read name =>
    match w.find name with
    | none => (w, .docNotFound)
    | some d => (w, .read d.body d.version)
  | .history name =>
    match w.find name with
    | none => (w, .docNotFound)
    | some d => (w, .history (d.history.reverse.map fun p => (p.kind, p.versionBefore, p.versionAfter)))

end Memory
