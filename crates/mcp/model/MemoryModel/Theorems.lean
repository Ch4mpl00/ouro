/-
What is proved about the model, for every world and every call — not for
examples. The statements are the part to read; the proofs are checked by
Lean's kernel and need no review.

  1. failed_call_changes_nothing — an error answer leaves every document as it was
  2. versions_match_history      — version n means exactly n recorded patches,
                                   patch i moved the document from i to i+1
  3. revert_newest_restores      — reverting the newest patch restores the body it replaced
  4. patch_then_revert           — a patch followed by its revert is a no-op on the text
  5. append_keeps_text           — append keeps the old text and contains the new
  6. newest_is_current_version   — "the newest patch" and "the patch that produced
                                   the current version" are the same patch, so an
                                   implementation may decide either way
-/
import MemoryModel.Model

namespace Memory

/-! ## Statements -/

/-- The invariant behind version numbers. -/
def Doc.WellVersioned (d : Doc) : Prop :=
  d.version = d.history.length ∧
  ∀ i p, d.history[i]? = some p → p.versionBefore = i ∧ p.versionAfter = i + 1

def World.WellVersioned (w : World) : Prop :=
  ∀ name d, w.find name = some d → d.WellVersioned

/-- Every world the calls can produce, starting from no documents. -/
inductive Reachable : World → Prop
  | empty : Reachable []
  | step (w : World) (c : Cmd) : Reachable w → Reachable (step w c).1

/-! ## Lemmas about the world map -/

theorem find_filter_ne (w : World) (n m : String) (h : m ≠ n) :
    (w.filter (·.1 != n)).find? (·.1 == m) = w.find? (·.1 == m) := by
  induction w with
  | nil => rfl
  | cons x xs ih =>
    by_cases hx : x.1 = n
    · have hm : (n == m) = false := by simpa using fun e => h e.symm
      simp [hx, hm, ih]
    · simp [List.find?_cons, hx]
      split <;> simp_all

theorem find_set (w : World) (n m : String) (d : Doc) :
    (w.set n d).find m = if m = n then some d else w.find m := by
  unfold World.set World.find
  by_cases h : m = n
  · subst h; simp
  · have hn : n ≠ m := fun e => h e.symm
    simp [h, hn, find_filter_ne w n m h]

theorem commit_reply (w : World) (n : String) (d : Doc) (k : Kind) (es : List Edit) (b : Text) :
    (commit w n d k es b).2 = .written (d.version + 1) (sizeBytes b) := rfl

theorem commit_find (w : World) (n : String) (d : Doc) (k : Kind) (es : List Edit) (b : Text) :
    (commit w n d k es b).1.find n =
      some { body := b, version := d.version + 1,
             history := d.history ++ [{ kind := k, edits := es, bodyBefore := d.body,
                                        versionBefore := d.version, versionAfter := d.version + 1 }] } := by
  simp [commit, find_set]

/-! ## 1. Failure is atomic -/

theorem failed_call_changes_nothing (w : World) (c : Cmd) :
    (step w c).2.isError → (step w c).1 = w := by
  cases c <;> simp only [step, write, append, patch, revert] <;>
    repeat (first | split | simp [commit_reply, Reply.isError] | rfl)

/-! ## 2. Versions -/

theorem empty_wellVersioned : Doc.empty.WellVersioned := by
  simp [Doc.WellVersioned, Doc.empty]

theorem commit_world (w : World) (n : String) (d : Doc) (k : Kind) (es : List Edit) (b : Text)
    (hw : w.WellVersioned) (hd : d.WellVersioned) : (commit w n d k es b).1.WellVersioned := by
  intro m d' hm
  simp only [commit, find_set] at hm
  split at hm
  · cases hm
    obtain ⟨hv, hi⟩ := hd
    refine ⟨by simp [hv], fun i p hp => ?_⟩
    rw [List.getElem?_append] at hp
    split at hp
    · exact hi i p hp
    · have hle : d.history.length ≤ i := by omega
      cases hj : i - d.history.length with
      | zero =>
        simp [hj] at hp; subst hp
        have : i = d.history.length := by omega
        simp [this, hv]
      | succ j => simp [hj] at hp
  · exact hw m d' hm

theorem step_wellVersioned (w : World) (c : Cmd) (hw : w.WellVersioned) : (step w c).1.WellVersioned := by
  cases c <;> simp only [step, write, append, patch, revert] <;> repeat' split
  all_goals first
    | exact hw
    | exact commit_world _ _ _ _ _ _ hw empty_wellVersioned
    | exact commit_world _ _ _ _ _ _ hw (hw _ _ (by assumption))

theorem versions_match_history {w : World} (h : Reachable w) : w.WellVersioned := by
  induction h with
  | empty => intro n d hd; simp [World.find] at hd
  | step w c _ ih => exact step_wellVersioned w c ih

theorem newest_is_current_version {w : World} (h : Reachable w) (n : String) (d : Doc) (i : Nat) (p : Patch)
    (hd : w.find n = some d) (hp : d.history[i]? = some p) :
    i + 1 = d.history.length ↔ p.versionAfter = d.version := by
  obtain ⟨hv, hi⟩ := versions_match_history h n d hd
  rw [(hi i p hp).2, hv]

/-! ## 3–4. Revert -/

theorem revert_newest_restores (w : World) (n : String) (d : Doc) (earlier : List Patch) (p : Patch) (rollback : Bool)
    (hd : w.find n = some d) (hh : d.history = earlier ++ [p]) :
    ((step w (.revert n earlier.length rollback)).1.find n).map (·.body) = some p.bodyBefore := by
  simp [step, revert, hd, hh, commit_find]

theorem patch_then_revert (w w' : World) (n : String) (d : Doc) (expected : Nat) (edits : List Edit)
    (version size : Nat) (hd : w.find n = some d)
    (hw : step w (.patch n expected edits) = (w', .written version size)) :
    ((step w' (.revert n d.history.length false)).1.find n).map (·.body) = some d.body := by
  simp only [step, patch, hd] at hw
  repeat' (split at hw)
  all_goals (try simp at hw)
  rename_i body _
  have h' : w' = (commit w n d .patch edits body).1 := by rw [hw]
  subst h'
  exact revert_newest_restores _ n _ d.history _ false (commit_find ..) rfl

/-! ## 5. Append -/

theorem join_keeps_body (b a : Text) : trimEnd b <+: joinParagraphs b a := by
  cases h1 : (trimEnd b).isEmpty <;> cases h2 : a.isEmpty <;> simp only [joinParagraphs, h1, h2] <;>
    simp_all [List.isEmpty_iff]

theorem join_contains_addition (b a : Text) : a <:+: joinParagraphs b a := by
  cases h1 : (trimEnd b).isEmpty <;> cases h2 : a.isEmpty <;> simp only [joinParagraphs, h1, h2] <;>
    simp_all [List.isEmpty_iff]
  all_goals first
    | exact List.infix_append_left
    | exact ⟨trimEnd b ++ ['\n', '\n'], ['\n'], by simp⟩

theorem append_keeps_text (w : World) (n : String) (d : Doc) (t : Text) (ht : t ≠ []) (hd : w.find n = some d) :
    ∃ d', (step w (.append n t)).1.find n = some d' ∧ trimEnd d.body <+: d'.body ∧ trim t <:+: d'.body := by
  simp only [step, append, ht, hd, ite_false, commit_find]
  exact ⟨_, rfl, join_keeps_body .., join_contains_addition ..⟩

end Memory
