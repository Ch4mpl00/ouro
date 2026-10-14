/-
The model as a process, for the Rust harness (tests/model.rs). JSON lines on
stdin, one answer per line on stdout:

  {"reset": true}                  → {"ok": true}            empty world
  {"step": <call>}                 → <answer>                run one call, keep the world
  {"linearizable": [<event>, …]}   → {"linearizable": bool}  check a concurrent history
                                                             against the current world

  <call>  = {"op": "write", "doc", "body", "expected"?} | {"op": "append", "doc", "text"}
          | {"op": "patch", "doc", "expected", "edits": [{"old", "new"}]}
          | {"op": "revert", "doc", "index", "rollback"} | {"op": "read" | "history", "doc"}
  <event> = {"call": <call>, "answer": <answer>, "start": µs, "end": µs}
-/
import MemoryModel
import Lean.Data.Json

open Lean Memory

def kindName : Kind → String
  | .write => "write" | .append => "append" | .patch => "patch" | .revert => "revert"

def text (t : Text) : Json := Json.str (String.ofList t)

def failure (code : String) (extra : List (String × Json) := []) : Json :=
  Json.mkObj ([("ok", (false : Json)), ("error", (code : Json))] ++ extra)

def render : Reply → Json
  | .written v s => Json.mkObj [("ok", true), ("version", v), ("sizeBytes", s)]
  | .read b v => Json.mkObj [("ok", true), ("body", text b), ("version", v)]
  | .history es => Json.mkObj [("ok", true), ("history", Json.arr (es.toArray.map fun (k, vb, va) =>
      Json.mkObj [("kind", kindName k), ("versionBefore", vb), ("versionAfter", va)]))]
  | .docNotFound => failure "doc_not_found"
  | .versionRequired c => failure "version_required" [("currentVersion", c)]
  | .versionConflict c => failure "version_conflict" [("currentVersion", c)]
  | .editFailed => failure "edit_failed"
  | .patchNotFound => failure "patch_not_found"
  | .revertConflict v => failure "revert_conflict" [("rollbackToVersion", v)]
  | .rejected => Json.mkObj [("rejected", true)]

def parseCmd (j : Json) : Except String Cmd := do
  let doc ← j.getObjValAs? String "doc"
  let str (k : String) : Except String Text := (j.getObjValAs? String k).map String.toList
  match ← j.getObjValAs? String "op" with
  | "write" =>
    let expected := (j.getObjValAs? Nat "expected").toOption
    return .write doc (← str "body") expected
  | "append" => return .append doc (← str "text")
  | "patch" =>
    let edits ← (← j.getObjValAs? (Array Json) "edits").toList.mapM fun e => do
      return { old := (← e.getObjValAs? String "old").toList, new := (← e.getObjValAs? String "new").toList }
    return .patch doc (← j.getObjValAs? Nat "expected") edits
  | "revert" => return .revert doc (← j.getObjValAs? Nat "index") (← j.getObjValAs? Bool "rollback")
  | "read" => return .read doc
  | "history" => return .history doc
  | op => throw s!"unknown op {op}"

def parseEvent (j : Json) : Except String Event := do
  return {
    cmd := ← parseCmd (← j.getObjVal? "call")
    answer := (← j.getObjVal? "answer").compress
    start := ← j.getObjValAs? Nat "start"
    finish := ← j.getObjValAs? Nat "end" }

def handle (w : World) (j : Json) : Except String (World × Json) := do
  if (j.getObjVal? "reset").isOk then return ([], Json.mkObj [("ok", true)])
  if let .ok call := j.getObjVal? "step" then
    let (w', reply) := step w (← parseCmd call)
    return (w', render reply)
  let events ← (← j.getObjValAs? (Array Json) "linearizable").mapM parseEvent
  return (w, Json.mkObj [("linearizable", linearizable (fun r => (render r).compress) events w)])

partial def loop (stdin : IO.FS.Stream) (stdout : IO.FS.Stream) (w : World) : IO Unit := do
  let line ← stdin.getLine
  if line.isEmpty then return
  let (w', out) := match Json.parse line >>= handle w with
    | .ok r => r
    | .error e => (w, Json.mkObj [("protocolError", e)])
  stdout.putStrLn out.compress
  stdout.flush
  loop stdin stdout w'

def main : IO Unit := do
  loop (← IO.getStdin) (← IO.getStdout) []
