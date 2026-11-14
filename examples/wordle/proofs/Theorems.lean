import Spec
import H5iAppLib
import H5iAppLib.ReadOnly
/-!
# Proofs about the extracted Wordle-v0 kernel

Each theorem is about `generated/WordleKernel.lean`, which `h5i app extract`
produces from `kernel/src/lib.rs`:

* `mark` is the TextArena marking rule of `Spec.mark`;
* a guess wins exactly when it is the secret, and every reward is in 0..1000;
* a valid guess pays `Spec.guessReward`, and an invalid move never pays more
  than the completion of the last real guess;
* every game ends: each move either ends it or uses up one of
  `Spec.movesLeft`, so no game lasts more than twelve moves;
* a finished game refuses every move, and no move changes the secret;
* the kernel never panics, and `State` writes nothing.

The dictionary, the secret chosen for a seed, message parsing, and the
transcript are trusted shell code and are not covered.
-/
open Aeneas Aeneas.Std Result Aeneas.Std.WP H5iAppLib wordle_kernel wordle_kernel.Spec

h5i_derive_all

namespace wordle_kernel.Theorems

/-! ## Marking -/

/-- For words of letters below 26, `mark` is `Spec.mark`. -/
@[step]
theorem mark_spec (secret guess : Array U8 5#usize)
    (hs : ∀ x ∈ secret.val, x.val < 26) (hg : ∀ x ∈ guess.val, x.val < 26) :
    mark secret guess ⦃ m => m.val = Spec.mark (word secret) (word guess) ⦄ := by
  unfold mark mark_loop0 mark_loop1 mark_loop1_loop0
  sorry

/-- All five marks are green exactly when the guess is the secret. -/
theorem all_green_iff (secret guess : Array U8 5#usize)
    (hs : ∀ x ∈ secret.val, x.val < 26) (hg : ∀ x ∈ guess.val, x.val < 26) :
    (do let m ← mark secret guess; all_green m) ⦃ b => b = true ↔ word guess = word secret ⦄ := by
  sorry

/-- A completion is a multiple of 100 between 0 and 1000. -/
@[step]
theorem completion_spec (marks : Array Mark 5#usize) :
    completion_milli marks ⦃ r => r.val = Spec.completion marks.val ∧ 0 ≤ r.val ∧ r.val ≤ 1000 ⦄ := by
  sorry

/-! ## Rewards -/

/-- Every reward a step commits is between 0 and 1000. -/
theorem reward_in_range (c : Core) (mv : Move) :
    transition () c (.Step mv) ⦃ r => ∀ w rep, r = .Ok (w, rep) →
      0 ≤ rep.reward_milli.val ∧ rep.reward_milli.val ≤ 1000 ⦄ := by
  sorry

/-- A valid guess pays `Spec.guessReward`; it wins exactly when it is the secret. -/
theorem guess_pays (c : Core) (letters : Array U8 5#usize) (hw : ∀ x ∈ c.secret.val, x.val < 26) :
    transition () c (.Step (.Candidate letters true)) ⦃ r => ∀ c' rep phase,
      r = .Ok (some c', rep) → rep.phase = some phase →
      (∃ marks left outcome, phase = .Played letters marks left outcome) →
      rep.reward_milli.val = Spec.guessReward (word c.secret) (word letters) (c'.guess_count.val = 6) ∧
      (c'.won = true ↔ word letters = word c.secret) ⦄ := by
  sorry

/-! ## Games end -/

/-- Each move ends the game or uses up one of `Spec.movesLeft`, which starts
at 12. -/
theorem game_ends (c : Core) (mv : Move) :
    transition () c (.Step mv) ⦃ r => ∀ c' rep, r = .Ok (some c', rep) →
      c'.done = true ∨ movesLeft c' < movesLeft c ⦄ := by
  sorry

/-- A fresh game has twelve moves left. -/
theorem fresh_moves (secret : Array U8 5#usize) :
    fresh secret ⦃ c => movesLeft c = 12 ⦄ := by
  unfold fresh; simp [movesLeft]

/-- A finished game refuses every move and commits nothing. -/
theorem done_is_final (c : Core) (mv : Move) (hd : c.done = true) :
    transition () c (.Step mv) = ok (.Err .Done) := by
  unfold transition step_core
  simp [hd]

/-- No move changes the secret. -/
theorem secret_is_fixed (c : Core) (mv : Move) :
    transition () c (.Step mv) ⦃ r => ∀ c' rep, r = .Ok (some c', rep) → c'.secret = c.secret ⦄ := by
  sorry

/-! ## Obligations `h5i app check` gates -/

theorem transition_total (a : Unit) (c : Core) (cmd : Command) :
    ∃ r, transition a c cmd = ok r := by
  sorry

theorem panic_free : H5iAppLib.Obligation.PanicFree transition :=
  H5iAppLib.Obligation.panicFree_of_total transition_total

def writesOf : Option Core → List Core
  | none => []
  | some c => [c]

theorem read_only_sound : H5iAppLib.ReadOnly.Sound read_only transition writesOf := by
  apply H5iAppLib.ReadOnly.sound_of_spec
  intro a c cmd hro
  cases cmd <;> simp [read_only] at hro
  unfold transition
  simp [H5iAppLib.OnOk, writesOf]

end wordle_kernel.Theorems
