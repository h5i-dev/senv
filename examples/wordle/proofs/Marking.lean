import Spec
/-!
# The two-pass marker, on lists

`mark` in the kernel works on arrays: a green pass that replaces each matched
secret letter with 255, then a yellow pass that takes the first remaining copy
of each other letter and replaces it with 255. This file states that
algorithm on lists (`Model.mark`) and proves it equals `Spec.mark`, the
TextArena rule written with letter counts. `Theorems.lean` proves the
extracted Rust computes `Model.mark`.
-/
open wordle_kernel wordle_kernel.Spec

namespace wordle_kernel.Model

/-- The marker for a used-up secret letter. Letters are below 26. -/
abbrev used : Nat := 255

/-- The green pass: green where the guess matches the secret. -/
def greens : List Nat → List Nat → List Mark
  | s :: ss, g :: gs => (if g = s then .Green else .Wrong) :: greens ss gs
  | _, _ => []

/-- What is left of the secret after the green pass. -/
def left : List Nat → List Nat → List Nat
  | s :: ss, g :: gs => (if g = s then used else s) :: left ss gs
  | _, _ => []

/-- Replace the first `x` with `used`. -/
def consume : List Nat → Nat → List Nat
  | [], _ => []
  | y :: ys, x => if y = x then used :: ys else y :: consume ys x

/-- The yellow pass, left to right over the guess and the green marks. -/
def yellows : List Nat → List Mark → List Nat → List Mark
  | g :: gs, m :: ms, rest =>
    if isGreen m then .Green :: yellows gs ms rest
    else if g ∈ rest then .Yellow :: yellows gs ms (consume rest g)
    else m :: yellows gs ms rest
  | _, _, _ => []

def mark (secret guess : List Nat) : List Mark :=
  yellows guess (greens secret guess) (left secret guess)

/-! ## Counting -/

theorem count_consume (l : List Nat) (x y : Nat) (hy : y ≠ used) (hx : x ∈ l) :
    (consume l x).count y + (if y = x then 1 else 0) = l.count y := by
  induction l with
  | nil => simp at hx
  | cons z zs ih =>
    by_cases hz : z = x
    · subst hz
      simp only [consume, if_true, List.count_cons, beq_iff_eq]
      have : ¬ used = y := fun h => hy h.symm
      simp only [this, if_false]
      by_cases hyz : y = z
      · simp [hyz]
      · simp [hyz, Ne.symm hyz]
    · have hx' : x ∈ zs := by
        rcases List.mem_cons.1 hx with h | h
        · exact absurd h.symm hz
        · exact h
      simp only [consume, hz, if_false, List.count_cons, beq_iff_eq]
      have := ih hx'
      by_cases hyx : y = x <;> by_cases hzy : z = y <;> simp_all <;> omega

theorem consume_bound (l : List Nat) (x : Nat) (h : ∀ y ∈ l, y < 26 ∨ y = used) :
    ∀ y ∈ consume l x, y < 26 ∨ y = used := by
  induction l with
  | nil => simp [consume]
  | cons z zs ih =>
    simp only [consume]
    split <;> intro y hy <;> simp at hy
    · rcases hy with rfl | hy
      · right; rfl
      · exact h y (by simp [hy])
    · rcases hy with rfl | hy
      · exact h y (by simp)
      · exact ih (fun y hy => h y (by simp [hy])) y hy

/-- The yellow pass agrees with `Spec.markPairs` when the copies left in
`rest` are the counts `rem`. -/
theorem yellows_eq (ss gs : List Nat) (rest : List Nat) (rem : Nat → Nat)
    (hlen : ss.length = gs.length)
    (hg : ∀ x ∈ gs, x < 26) (hb : ∀ y ∈ rest, y < 26 ∨ y = used)
    (hc : ∀ x, x < 26 → rest.count x = rem x) :
    yellows gs (greens ss gs) rest = markPairs (ss.zip gs) rem := by
  induction ss generalizing gs rest rem with
  | nil => cases gs <;> simp_all [greens, yellows, markPairs]
  | cons s ss ih =>
    cases gs with
    | nil => simp at hlen
    | cons g gs =>
      have hg0 : g < 26 := hg g (by simp)
      have hgs : ∀ x ∈ gs, x < 26 := fun x hx => hg x (by simp [hx])
      simp only [greens, yellows, List.zip_cons_cons, markPairs]
      by_cases hsg : g = s
      · subst hsg
        simp [isGreen, ih gs rest rem (by simpa using hlen) hgs hb hc]
      · have hsg' : ¬ s = g := fun h => hsg h.symm
        simp only [hsg, if_false, hsg', isGreen, Bool.false_eq_true]
        have hmem : g ∈ rest ↔ 0 < rem g := by rw [← hc g hg0, List.count_pos_iff]
        by_cases hin : g ∈ rest
        · have hpos := hmem.1 hin
          simp only [hin, if_true, hpos]
          congr 1
          apply ih gs (consume rest g) (take rem g) (by simpa using hlen) hgs (consume_bound rest g hb)
          intro x hx
          have hcx := count_consume rest g x (by simp [used]; omega) hin
          rw [hc x hx] at hcx
          simp only [take]
          split_ifs at hcx ⊢ <;> omega
        · have hz : ¬ 0 < rem g := fun h => hin (hmem.2 h)
          simp only [hin, if_false, hz]
          congr 1
          exact ih gs rest rem (by simpa using hlen) hgs hb hc

theorem countP_le (ss gs : List Nat) (x : Nat) :
    ((ss.zip gs).countP fun p => decide (p.1 = x ∧ p.2 = x)) ≤ ss.count x := by
  induction ss generalizing gs with
  | nil => simp
  | cons a rest iha =>
    cases gs with
    | nil => simp
    | cons b bs =>
      have := iha bs
      simp only [List.zip_cons_cons, List.countP_cons, List.count_cons, beq_iff_eq]
      simp only [Bool.decide_and] at this ⊢
      by_cases ha : a = x <;> by_cases hb : b = x <;> simp [ha, hb] <;> omega

theorem count_left (ss gs : List Nat) (hlen : ss.length = gs.length) (x : Nat) (hx : x ≠ used) :
    (left ss gs).count x = afterGreens ss gs x := by
  induction ss generalizing gs with
  | nil => cases gs <;> simp [left, afterGreens]
  | cons s ss ih =>
    cases gs with
    | nil => simp at hlen
    | cons g gs =>
      have ih' := ih gs (by simpa using hlen)
      have hle := countP_le ss gs x
      simp only [afterGreens] at ih' ⊢
      simp only [left, List.zip_cons_cons, List.countP_cons, List.count_cons, beq_iff_eq, decide_eq_true_eq]
      have hu : ¬ used = x := fun h => hx h.symm
      by_cases h1 : g = s <;> by_cases h2 : s = x <;> simp_all <;> omega

theorem left_bound (ss gs : List Nat) (hs : ∀ x ∈ ss, x < 26) :
    ∀ y ∈ left ss gs, y < 26 ∨ y = used := by
  induction ss generalizing gs with
  | nil => simp [left]
  | cons s ss ih =>
    cases gs with
    | nil => simp [left]
    | cons g gs =>
      simp only [left, List.mem_cons]
      rintro y (rfl | hy)
      · split
        · right; rfl
        · left; exact hs s (by simp)
      · exact ih gs (fun x hx => hs x (by simp [hx])) y hy

/-- The list algorithm is the TextArena rule. -/
theorem mark_eq_spec (secret guess : List Nat) (hlen : secret.length = guess.length)
    (hs : ∀ x ∈ secret, x < 26) (hg : ∀ x ∈ guess, x < 26) :
    mark secret guess = Spec.mark secret guess := by
  unfold mark Spec.mark
  apply yellows_eq secret guess _ _ hlen hg (left_bound secret guess hs)
  intro x hx
  exact count_left secret guess hlen x (by simp [used]; omega)

end wordle_kernel.Model
