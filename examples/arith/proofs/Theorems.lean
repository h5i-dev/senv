import Spec
import H5iAppLib
import H5iAppLib.ReadOnly
import Mathlib.Tactic.IntervalCases
import Mathlib.Tactic.SplitIfs
/-!
# Proofs about the extracted Arith-v0 kernel

Each theorem is about `generated/ArithKernel.lean`, which `h5i app extract`
produces from `kernel/src/lib.rs`. Together they say that the reward an agent
can earn is exactly the one `Spec.lean` describes:

* the calculator computes the integer operation, and only inside its range;
* the kernel's value of a valid problem is `Spec.value`, with no overflow;
* an answer pays 1 exactly when it equals that value, and nothing else pays;
* every episode ends within `MAX_STEPS` steps, and a finished one stays finished;
* no command can change the problem, so validity holds for the whole episode;
* the kernel never panics, and `State` writes nothing.

None of this covers the server: text parsing, problem generation from the
seed, and the rendering of observations are trusted shell code.
-/
open Aeneas Aeneas.Std Result Aeneas.Std.WP H5iAppLib arith_kernel arith_kernel.Spec

namespace arith_kernel.Theorems

theorem mul_bound (x y : Int) (hx : InRange x) (hy : InRange y) :
    -1000000000000000000 ≤ x * y ∧ x * y ≤ 1000000000000000000 := by
  obtain ⟨hx1, hx2⟩ := hx; obtain ⟨hy1, hy2⟩ := hy
  constructor <;> nlinarith

theorem operand_ok_eq (x : I64) : operand_ok x = ok (decide (InRange x.val)) := by
  unfold operand_ok
  simp only [MIN_OPERAND, MAX_OPERAND, InRange]
  split <;> simp_all

theorem calc_spec (a : I64) (o : Op) (b : I64) :
    «calc» a o b ⦃ r =>
      (InRange a.val ∧ InRange b.val → ∃ v, r = some v ∧ v.val = opVal o a.val b.val) ∧
      (¬ (InRange a.val ∧ InRange b.val) → r = none) ⦄ := by
  unfold «calc»
  simp only [operand_ok_eq]
  by_cases ha : InRange a.val <;> by_cases hb : InRange b.val
  · have hm := mul_bound _ _ ha hb
    obtain ⟨ha1, ha2⟩ := ha; obtain ⟨hb1, hb2⟩ := hb
    cases o <;> step* <;> simp_all [opVal, InRange]
  all_goals step*

/-- One more operation by a digit grows the bound from `m` to `9 m + 9`. -/
theorem opVal_bound (o : Op) (x y m : Int) (hx : -m ≤ x ∧ x ≤ m) (hy : 1 ≤ y ∧ y ≤ 9) (hm : 0 ≤ m) :
    -(9 * m + 9) ≤ opVal o x y ∧ opVal o x y ≤ 9 * m + 9 := by
  obtain ⟨hx1, hx2⟩ := hx; obtain ⟨hy1, hy2⟩ := hy
  cases o <;> simp only [opVal] <;> constructor <;> nlinarith

/-- The kernel's `calc` on operands in range, as a value. -/
theorem calc_ok (a : I64) (o : Op) (b : I64) (ha : InRange a.val) (hb : InRange b.val) :
    ∃ v : I64, «calc» a o b = ok (some v) ∧ v.val = opVal o a.val b.val := by
  have h := calc_spec a o b
  obtain ⟨r, hr, hpost⟩ := (WP.spec_equiv_exists _ _).1 h
  obtain ⟨v, rfl, hv⟩ := hpost.1 ⟨ha, hb⟩
  exact ⟨v, hr, hv⟩

theorem then_some (a : I64) (o : Op) (b : I64) : «then» (some a) o b = «calc» a o b := by
  simp [«then»]

theorem getD_digit (p : Problem) (i : Nat) (hi : i < 5) :
    (p.digits.val[i]'(by simp; omega)).val = digit p i := by
  simp [digit, List.getD_eq_getElem?_getD, hi]

theorem getD_op (p : Problem) (i : Nat) (hi : i < 4) :
    p.ops.val[i]'(by simp; omega) = op p i := by
  simp [op, List.getD_eq_getElem?_getD, hi]

theorem index_ok {α : Type} {n : Usize} (v : Array α n) (i : Usize) (h : i.val < n.val) :
    Array.index_usize v i = ok (v.val[i.val]'(by simp [h])) := by
  have := Array.index_usize_spec v i (by simp [h])
  obtain ⟨r, hr, hx⟩ := (WP.spec_equiv_exists _ _).1 this
  rw [hr, hx]

theorem value_spec (p : Problem) (hp : Valid p) :
    arith_kernel.value p ⦃ r => ∃ v, r = some v ∧ v.val = Spec.value p ⦄ := by
  obtain ⟨hlo, hhi, hd⟩ := hp
  have dv : ∀ i (h : i < 5), 1 ≤ (p.digits.val[i]'(by simp; omega)).val ∧
      (p.digits.val[i]'(by simp; omega)).val ≤ 9 := by
    intro i h; rw [getD_digit p i h]; exact hd i h
  have inr : ∀ i (h : i < 5), InRange (p.digits.val[i]'(by simp; omega)).val := by
    intro i h; have := dv i h; constructor <;> omega
  unfold arith_kernel.value
  simp only [index_ok _ _ (by simp : (0#usize).val < (5#usize).val), index_ok _ _ (by simp : (1#usize).val < (5#usize).val),
    index_ok _ _ (by simp : (2#usize).val < (5#usize).val), index_ok _ _ (by simp : (3#usize).val < (5#usize).val),
    index_ok _ _ (by simp : (4#usize).val < (5#usize).val), index_ok _ _ (by simp : (0#usize).val < (4#usize).val),
    index_ok _ _ (by simp : (1#usize).val < (4#usize).val), index_ok _ _ (by simp : (2#usize).val < (4#usize).val),
    index_ok _ _ (by simp : (3#usize).val < (4#usize).val), Std.bind_ok]
  have z0 : (0#usize).val = 0 := by simp
  have z1 : (1#usize).val = 1 := by simp
  have z2 : (2#usize).val = 2 := by simp
  have z3 : (3#usize).val = 3 := by simp
  have z4 : (4#usize).val = 4 := by simp
  simp only [z0, z1, z2, z3, z4]
  obtain ⟨v1, h1, e1⟩ := calc_ok _ (p.ops.val[0]'(by simp)) _ (inr 0 (by omega)) (inr 1 (by omega))
  have d0 := dv 0 (by omega)
  have b1 := opVal_bound (p.ops.val[0]'(by simp)) (p.digits.val[0]'(by simp)).val (p.digits.val[1]'(by simp)).val 9 (by constructor <;> omega) (dv 1 (by omega)) (by omega)
  rw [← e1] at b1
  obtain ⟨v2, h2, e2⟩ := calc_ok v1 (p.ops.val[1]'(by simp)) _ (by constructor <;> omega) (inr 2 (by omega))
  have b2 := opVal_bound (p.ops.val[1]'(by simp)) v1.val (p.digits.val[2]'(by simp)).val 90 (by omega) (dv 2 (by omega)) (by omega)
  rw [← e2] at b2
  obtain ⟨v3, h3, e3⟩ := calc_ok v2 (p.ops.val[2]'(by simp)) _ (by constructor <;> omega) (inr 3 (by omega))
  have b3 := opVal_bound (p.ops.val[2]'(by simp)) v2.val (p.digits.val[3]'(by simp)).val 819 (by omega) (dv 3 (by omega)) (by omega)
  rw [← e3] at b3
  obtain ⟨v4, h4, e4⟩ := calc_ok v3 (p.ops.val[3]'(by simp)) _ (by constructor <;> omega) (inr 4 (by omega))
  simp only [h1, Std.bind_ok, then_some, h2, h3, h4]
  have n234 : p.n_ops.val = 2 ∨ p.n_ops.val = 3 ∨ p.n_ops.val = 4 := by omega
  rcases n234 with hn | hn | hn
  · have : p.n_ops = 2#u8 := by scalar_tac
    simp only [this, if_true, WP.spec_ok]
    refine ⟨v2, rfl, ?_⟩
    rw [e2, e1]
    unfold Spec.value; rw [hn]
    simp only [List.range_succ, List.range_zero, List.nil_append, List.foldl_append, List.foldl_cons,
      List.foldl_nil, Nat.zero_add]
    simp only [← getD_digit p _ (by omega : 0 < 5), ← getD_digit p _ (by omega : 1 < 5),
      ← getD_digit p _ (by omega : 2 < 5), ← getD_op p _ (by omega : 0 < 4), ← getD_op p _ (by omega : 1 < 4)]
  · have h2' : p.n_ops ≠ 2#u8 := by scalar_tac
    have h3' : p.n_ops = 3#u8 := by scalar_tac
    simp only [if_neg h2', if_pos h3', WP.spec_ok]
    refine ⟨v3, rfl, ?_⟩
    rw [e3, e2, e1]
    unfold Spec.value; rw [hn]
    simp only [List.range_succ, List.range_zero, List.nil_append, List.foldl_append, List.foldl_cons,
      List.foldl_nil, Nat.zero_add]
    simp only [← getD_digit p _ (by omega : 0 < 5), ← getD_digit p _ (by omega : 1 < 5),
      ← getD_digit p _ (by omega : 2 < 5), ← getD_op p _ (by omega : 0 < 4), ← getD_op p _ (by omega : 1 < 4)]
    simp only [← getD_digit p _ (by omega : 3 < 5), ← getD_op p _ (by omega : 2 < 4)]
  · have h2' : p.n_ops ≠ 2#u8 := by scalar_tac
    have h3' : p.n_ops ≠ 3#u8 := by scalar_tac
    simp only [if_neg h2', if_neg h3', WP.spec_ok]
    refine ⟨v4, rfl, ?_⟩
    rw [e4, e3, e2, e1]
    unfold Spec.value; rw [hn]
    simp only [List.range_succ, List.range_zero, List.nil_append, List.foldl_append, List.foldl_cons,
      List.foldl_nil, Nat.zero_add]
    simp only [← getD_digit p _ (by omega : 0 < 5), ← getD_digit p _ (by omega : 1 < 5),
      ← getD_digit p _ (by omega : 2 < 5), ← getD_op p _ (by omega : 0 < 4), ← getD_op p _ (by omega : 1 < 4)]
    all_goals try simp only [← getD_digit p _ (by omega : 3 < 5), ← getD_op p _ (by omega : 2 < 4)]
    all_goals try simp only [← getD_digit p _ (by omega : 4 < 5), ← getD_op p _ (by omega : 3 < 4)]

/-- The derived `==` on feedback, against `Correct`. -/
def isCorrect : Feedback → Bool
  | .Correct => true
  | _ => false

theorem eq_correct (f : Feedback) :
    Feedback.Insts.CoreCmpPartialEqFeedback.eq f .Correct = ok (isCorrect f) := by
  cases f <;> simp [Feedback.Insts.CoreCmpPartialEqFeedback.eq, Feedback.read_discriminant, isCorrect]

theorem judge_spec (p : Problem) (hp : Valid p) (v : I64) :
    judge p v ⦃ f => (f = .Correct ↔ v.val = Spec.value p) ∧ (f = .Correct ∨ f = .Incorrect) ⦄ := by
  unfold judge
  have h := value_spec p hp
  obtain ⟨r, hr, x, rfl, hx⟩ := (WP.spec_equiv_exists _ _).1 h
  rw [hr]
  simp only [Std.bind_ok]
  split
  · rename_i h; subst h; simp [hx]
  · rename_i h
    simp only [WP.spec_ok]
    simp
    intro he; exact h (by scalar_tac)

theorem calc_total (a : I64) (o : Op) (b : I64) : ∃ r, «calc» a o b = ok r := by
  obtain ⟨r, hr, -⟩ := (WP.spec_equiv_exists _ _).1 (calc_spec a o b); exact ⟨r, hr⟩

/-- A step's feedback is `Correct` exactly for the right answer. -/
theorem respond_spec (p : Problem) (hp : Valid p) (a : Action) :
    respond p a ⦃ f => (isCorrect f = true ↔ ∃ v, a = .Answer v ∧ v.val = Spec.value p) ⦄ := by
  unfold respond
  cases a with
  | Calc l o r =>
    obtain ⟨x, hx⟩ := calc_total l o r
    simp only []
    rw [hx]; simp only [Std.bind_ok]
    cases x <;> simp [isCorrect]
  | Answer v =>
    have h := judge_spec p hp v
    obtain ⟨f, hf, h1, h2⟩ := (WP.spec_equiv_exists _ _).1 h
    simp only []
    rw [hf]; simp only [WP.spec_ok]
    rcases h2 with rfl | rfl <;> simp_all [isCorrect]
  | Invalid => simp [isCorrect]

theorem then_total (acc : Option I64) (o : Op) (b : I64) : ∃ r, «then» acc o b = ok r := by
  cases acc with
  | none => exact ⟨none, by simp [«then»]⟩
  | some a => rw [then_some]; exact calc_total a o b

/-- `value` never fails, for any problem: out-of-range intermediates give `none`. -/
theorem value_total (p : Problem) : ∃ r, arith_kernel.value p = ok r := by
  unfold arith_kernel.value
  simp only [index_ok _ _ (by simp : (0#usize).val < (5#usize).val), index_ok _ _ (by simp : (1#usize).val < (5#usize).val),
    index_ok _ _ (by simp : (2#usize).val < (5#usize).val), index_ok _ _ (by simp : (3#usize).val < (5#usize).val),
    index_ok _ _ (by simp : (4#usize).val < (5#usize).val), index_ok _ _ (by simp : (0#usize).val < (4#usize).val),
    index_ok _ _ (by simp : (1#usize).val < (4#usize).val), index_ok _ _ (by simp : (2#usize).val < (4#usize).val),
    index_ok _ _ (by simp : (3#usize).val < (4#usize).val), Std.bind_ok]
  have z0 : (0#usize).val = 0 := by simp
  have z1 : (1#usize).val = 1 := by simp
  have z2 : (2#usize).val = 2 := by simp
  have z3 : (3#usize).val = 3 := by simp
  have z4 : (4#usize).val = 4 := by simp
  simp only [z0, z1, z2, z3, z4]
  obtain ⟨x1, h1⟩ := calc_total (p.digits.val[0]'(by simp)) (p.ops.val[0]'(by simp)) (p.digits.val[1]'(by simp))
  rw [h1]; simp only [Std.bind_ok]
  obtain ⟨x2, h2⟩ := then_total x1 (p.ops.val[1]'(by simp)) (p.digits.val[2]'(by simp))
  rw [h2]; simp only [Std.bind_ok]
  obtain ⟨x3, h3⟩ := then_total x2 (p.ops.val[2]'(by simp)) (p.digits.val[3]'(by simp))
  obtain ⟨x4, h4⟩ := then_total x3 (p.ops.val[3]'(by simp)) (p.digits.val[4]'(by simp))
  split
  · exact ⟨_, rfl⟩
  · rw [h3]; simp only [Std.bind_ok]
    split
    · exact ⟨_, rfl⟩
    · rw [h4]; exact ⟨_, rfl⟩

theorem respond_total (p : Problem) (a : Action) : ∃ f, respond p a = ok f := by
  unfold respond judge
  cases a with
  | Calc l o r => obtain ⟨x, hx⟩ := calc_total l o r; simp only []; rw [hx]; cases x <;> simp
  | Answer v =>
    obtain ⟨x, hx⟩ := value_total p
    simp only []; rw [hx]; simp only [Std.bind_ok]
    cases x with
    | none => simp
    | some y => simp only []; split <;> simp
  | Invalid => simp

def isAnswer : Action → Bool
  | .Answer _ => true
  | _ => false

theorem step_shape (s : Snapshot) (a : Action) (hd : s.done = false) (hs : s.steps.val < 8) :
    ∃ f, respond s.problem a = ok f ∧
    transition () s (.Step a) ⦃ r => ∃ s' rep, r = .Ok (some s', rep) ∧
      s'.problem = s.problem ∧ s'.steps.val = s.steps.val + 1 ∧ s'.last = f ∧
      s'.won = isCorrect f ∧
      rep.reward_milli.val = (if isCorrect f then 1000 else 0) ∧ s'.reward_milli = rep.reward_milli ∧
      rep.done = s'.done ∧ s'.done = (isAnswer a || decide (s.steps.val + 1 ≥ 8)) ⦄ := by
  obtain ⟨f, hf⟩ := respond_total s.problem a
  refine ⟨f, hf, ?_⟩
  unfold transition is_answer at_limit
  simp only [hd, Bool.false_eq_true, if_false, MAX_STEPS]
  have : ¬ (s.steps ≥ 8#u8) := by scalar_tac
  simp only [this, if_false]
  step*
  rw [hf]; simp only [Std.bind_ok, eq_correct]
  cases hc : isCorrect f <;> cases a <;> simp_all [isAnswer, WIN_MILLI]

theorem respond_correct (p : Problem) (hp : Valid p) (a : Action) (f : Feedback) (hf : respond p a = ok f) :
    isCorrect f = true ↔ ∃ v, a = .Answer v ∧ v.val = Spec.value p := by
  have h := respond_spec p hp a
  rw [hf] at h; simpa using h

/-- An answer ends the episode and pays 1 if and only if it is the value of
the problem; otherwise it pays 0. -/
theorem answer_pays_iff_correct (s : Snapshot) (v : I64)
    (hp : Valid s.problem) (hd : s.done = false) (hs : s.steps.val < 8) :
    transition () s (.Step (.Answer v)) ⦃ r => ∃ s' rep,
      r = .Ok (some s', rep) ∧ rep.done = true ∧
      (rep.reward_milli.val = 1000 ↔ v.val = Spec.value s.problem) ∧
      (rep.reward_milli.val = 0 ∨ rep.reward_milli.val = 1000) ⦄ := by
  obtain ⟨f, hf, h⟩ := step_shape s (.Answer v) hd hs
  have hc := respond_correct s.problem hp _ f hf
  apply WP.spec_mono h
  rintro r ⟨s', rep, rfl, -, -, -, -, hr, -, hdone, hdone'⟩
  refine ⟨s', rep, rfl, by simp [hdone, hdone', isAnswer], ?_, ?_⟩
  · cases e : isCorrect f <;> simp_all
  · cases e : isCorrect f <;> simp_all

/-- Nothing but the correct answer pays: a step with a nonzero reward was
`answer v` with `v` the value of the problem. -/
theorem pays_only_for_the_value (s : Snapshot) (a : Action) (hp : Valid s.problem) :
    transition () s (.Step a) ⦃ r => ∀ w rep, r = .Ok (w, rep) →
      rep.reward_milli.val ≠ 0 → ∃ v, a = .Answer v ∧ v.val = Spec.value s.problem ⦄ := by
  by_cases hd : s.done = true
  · unfold transition; simp [hd]
  by_cases hs : s.steps.val < 8
  · obtain ⟨f, hf, h⟩ := step_shape s a (by simpa using hd) hs
    have hc := respond_correct s.problem hp _ f hf
    apply WP.spec_mono h
    rintro r ⟨s', rep, rfl, -, -, -, -, hr, -, -, -⟩ w rep' he hne
    simp only [core.result.Result.Ok.injEq, Prod.mk.injEq] at he
    obtain ⟨-, rfl⟩ := he
    cases e : isCorrect f <;> simp_all
  · unfold transition
    have : s.steps ≥ MAX_STEPS := by simp [MAX_STEPS]; scalar_tac
    simp [hd, this]

/-- A step counts one, never passes `MAX_STEPS`, and the step that reaches it
ends the episode. -/
theorem episode_ends (s : Snapshot) (a : Action) :
    transition () s (.Step a) ⦃ r => ∀ s' rep, r = .Ok (some s', rep) →
      s'.steps.val = s.steps.val + 1 ∧ s'.steps.val ≤ 8 ∧
      (s'.steps.val = 8 → s'.done = true ∧ rep.done = true) ⦄ := by
  by_cases hd : s.done = true
  · unfold transition; simp [hd]
  by_cases hs : s.steps.val < 8
  · obtain ⟨f, hf, h⟩ := step_shape s a (by simpa using hd) hs
    apply WP.spec_mono h
    rintro r ⟨s', rep, rfl, -, hst, -, -, -, -, hdone, hdone'⟩ s'' rep' he
    simp only [core.result.Result.Ok.injEq, Prod.mk.injEq, Option.some.injEq] at he
    obtain ⟨rfl, rfl⟩ := he
    refine ⟨hst, by omega, fun h8 => ?_⟩
    have : s.steps.val + 1 ≥ 8 := by omega
    simp_all
  · unfold transition
    have : s.steps ≥ MAX_STEPS := by simp [MAX_STEPS]; scalar_tac
    simp [hd, this]

/-- A finished episode refuses every step and commits nothing. -/
theorem done_is_final (s : Snapshot) (a : Action) (hd : s.done = true) :
    transition () s (.Step a) = ok (.Err .Done) := by
  unfold transition
  simp [hd]

/-- A step commits the same problem it started with. -/
theorem task_is_fixed (s : Snapshot) (a : Action) :
    transition () s (.Step a) ⦃ r => ∀ s' rep, r = .Ok (some s', rep) → s'.problem = s.problem ⦄ := by
  by_cases hd : s.done = true
  · unfold transition; simp [hd]
  by_cases hs : s.steps.val < 8
  · obtain ⟨f, hf, h⟩ := step_shape s a (by simpa using hd) hs
    apply WP.spec_mono h
    rintro r ⟨s', rep, rfl, hpb, -⟩ s'' rep' he
    simp only [core.result.Result.Ok.injEq, Prod.mk.injEq, Option.some.injEq] at he
    obtain ⟨rfl, rfl⟩ := he
    exact hpb
  · unfold transition
    have : s.steps ≥ MAX_STEPS := by simp [MAX_STEPS]; scalar_tac
    simp [hd, this]

theorem digit_ok_eq (d : I64) : digit_ok d = ok (decide (1 ≤ d.val ∧ d.val ≤ 9)) := by
  unfold digit_ok
  split <;> simp_all

theorem valid_true (p : Problem) (h : valid p = ok true) : Valid p := by
  unfold valid at h
  simp only [index_ok _ _ (by simp : (0#usize).val < (5#usize).val), index_ok _ _ (by simp : (1#usize).val < (5#usize).val),
    index_ok _ _ (by simp : (2#usize).val < (5#usize).val), index_ok _ _ (by simp : (3#usize).val < (5#usize).val),
    index_ok _ _ (by simp : (4#usize).val < (5#usize).val), digit_ok_eq, Std.bind_ok] at h
  have z0 : (0#usize).val = 0 := by simp
  have z1 : (1#usize).val = 1 := by simp
  have z2 : (2#usize).val = 2 := by simp
  have z3 : (3#usize).val = 3 := by simp
  have z4 : (4#usize).val = 4 := by simp
  simp only [z0, z1, z2, z3, z4] at h
  split_ifs at h with c1 c2 c3 c4 c5 c6 <;> simp at h
  simp only [decide_eq_true_eq] at c3 c4 c5 c6 h
  refine ⟨by scalar_tac, by scalar_tac, fun i hi => ?_⟩
  interval_cases i <;> rw [← getD_digit p _ (by omega)] <;> assumption

theorem valid_total (p : Problem) : ∃ b, valid p = ok b := by
  unfold valid
  simp only [index_ok _ _ (by simp : (0#usize).val < (5#usize).val), index_ok _ _ (by simp : (1#usize).val < (5#usize).val),
    index_ok _ _ (by simp : (2#usize).val < (5#usize).val), index_ok _ _ (by simp : (3#usize).val < (5#usize).val),
    index_ok _ _ (by simp : (4#usize).val < (5#usize).val), digit_ok_eq, Std.bind_ok]
  split_ifs <;> exact ⟨_, rfl⟩

/-- `Reset` accepts only valid problems, so with `task_is_fixed` every
snapshot an episode reaches holds a valid problem. -/
theorem reset_is_valid (s : Snapshot) (p : Problem) :
    transition () s (.Reset p) ⦃ r => ∀ w rep, r = .Ok (w, rep) → Valid p ⦄ := by
  unfold transition
  obtain ⟨b, hv⟩ := valid_total p
  simp only []
  rw [hv]; simp only [Std.bind_ok]
  cases b
  · simp
  · have := valid_true p hv
    simp [fresh, this]

/-- The kernel never fails: no panic, overflow or bad index, for any input. -/
theorem transition_total (a : Unit) (s : Snapshot) (c : Command) :
    ∃ r, transition a s c = ok r := by
  cases c with
  | Reset p =>
    unfold transition
    obtain ⟨b, hv⟩ := valid_total p
    simp only []; rw [hv]; simp only [Std.bind_ok]
    cases b <;> simp [fresh]
  | Step act =>
    by_cases hd : s.done = true
    · unfold transition; simp [hd]
    by_cases hs : s.steps.val < 8
    · obtain ⟨f, -, h⟩ := step_shape s act (by simpa using hd) hs
      obtain ⟨r, hr, -⟩ := (WP.spec_equiv_exists _ _).1 h
      exact ⟨r, hr⟩
    · unfold transition
      have : s.steps ≥ MAX_STEPS := by simp [MAX_STEPS]; scalar_tac
      simp [hd, this]
  | State => unfold transition; simp

/-- Panic freedom, as `H5iAppLib.Obligation.PanicFree` states it. -/
theorem panic_free : H5iAppLib.Obligation.PanicFree transition :=
  H5iAppLib.Obligation.panicFree_of_total transition_total

def writesOf : Option Snapshot → List Snapshot
  | none => []
  | some s => [s]

/-- `State`, the only command `read_only` admits, writes nothing. -/
theorem read_only_sound : H5iAppLib.Obligation.ReadOnlySound read_only transition writesOf := by
  apply H5iAppLib.ReadOnly.sound_of_spec
  intro a s c hro
  cases c <;> simp [read_only] at hro
  unfold transition
  simp [H5iAppLib.OnOk, writesOf]

end arith_kernel.Theorems
