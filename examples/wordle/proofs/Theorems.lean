import Spec
import H5iAppLib
import H5iAppLib.ReadOnly
import Marking
/-!
# Proofs about the extracted Wordle-v0 kernel

Each theorem is about `generated/WordleKernel.lean`, which `h5i app extract`
produces from `kernel/src/lib.rs`:

* `mark` is the TextArena marking rule `Spec.mark` (via `Marking.lean`, which
  proves the two-pass array algorithm equals the rule stated with counts);
* a scored guess pays 1000 exactly when it is the secret, a losing guess pays
  less than 1000, and every reward is between 0 and 1000;
* a guess equal to an earlier guess of the game is never scored;
* every game ends: each move either ends it or uses up one of
  `Spec.movesLeft`, which starts at 12;
* `Reset` keeps only secrets of letters below 26, a finished game refuses every
  move, and no move changes the secret;
* the kernel never panics, and `State` writes nothing.

The dictionary, the secret chosen for a seed, message parsing, and the
transcript are trusted shell code and are not covered.
-/
open Aeneas Aeneas.Std Result Aeneas.Std.WP H5iAppLib wordle_kernel wordle_kernel.Spec

namespace wordle_kernel.Theorems

def weight : Mark → Nat
  | .Green => 200
  | .Yellow => 100
  | .Wrong => 0

theorem weight_le (m : Mark) : weight m ≤ 200 := by cases m <;> simp [weight]

theorem fold_cast (l : List Mark) (a : Nat) :
    l.foldl (fun acc m => acc + (weight m : Int)) (a : Int) = ((l.foldl (fun acc m => acc + weight m) a : Nat) : Int) := by
  induction l generalizing a with
  | nil => simp
  | cons x xs ih => simp only [List.foldl_cons]; rw [← ih]; push_cast; rfl

theorem fold_bound (l : List Mark) (a : Nat) :
    l.foldl (fun acc m => acc + weight m) a ≤ a + 200 * l.length := by
  induction l generalizing a with
  | nil => simp
  | cons x xs ih =>
    simp only [List.foldl_cons, List.length_cons]
    have := ih (a + weight x); have := weight_le x; omega

theorem fold_completion (l : List Mark) (a : Nat) :
    l.foldl (fun acc m => acc + weight m) a = a + Spec.completion l := by
  induction l generalizing a with
  | nil => simp [Spec.completion]
  | cons x xs ih =>
    simp only [List.foldl_cons]; rw [ih]
    cases x <;> simp [Spec.completion, weight, isGreen, isYellow] <;> omega

@[step]
theorem completion_milli_spec (marks : Array Mark 5#usize) :
    completion_milli marks ⦃ r => r.val = (marks.val.foldl (fun acc m => acc + weight m) 0 : Nat) ∧
      0 ≤ r.val ∧ r.val ≤ 1000 ⦄ := by
  unfold completion_milli completion_milli_loop
  have hl : marks.val.length = 5 := by simp
  apply WP.spec_mono (loop_fold marks.val (fun t : I64 => t.val) (fun acc m => acc + (weight m : Int))
    (fun t i => 0 ≤ t.val ∧ t.val ≤ 200 * i) (fun x => completion_milli_loop.body marks x.1 x.2) ?_ 0#i64 0#usize
    (by simp) (by simp))
  · intro r hr
    have h0 : ((0#i64 : I64).val) = ((0 : Nat) : Int) := by simp
    have d0 : (0#usize : Usize).val = 0 := by simp
    simp only [d0, List.drop_zero, h0, fold_cast] at hr
    have hb := fold_bound marks.val 0
    rw [hl] at hb
    refine ⟨hr, by rw [hr]; omega, by rw [hr]; omega⟩
  · intro t j hj hI
    unfold completion_milli_loop.body
    simp only [WORD_LEN]
    split
    · rename_i hlt
      have hj5 : j.val < 5 := by scalar_tac
      obtain ⟨h0, h1⟩ := hI
      step*
      cases m <;> step* <;> simp only [FoldStep] <;> refine ⟨by omega, ?_, ?_, ?_, ?_⟩ <;>
        (try rw [← m_post]) <;> simp_all [weight] <;> scalar_tac
    · simp only [WP.spec_ok, FoldStep]; constructor <;> simp_all

@[step]
theorem mark_eq_spec (a b : Mark) :
    Mark.Insts.CoreCmpPartialEqMark.eq a b ⦃ r => r = true ↔ a = b ⦄ := by
  cases a <;> cases b <;> simp [Mark.Insts.CoreCmpPartialEqMark.eq, Mark.read_discriminant]

theorem word_len : WORD_LEN.val = 5 := by simp [WORD_LEN]
theorem word_len_eq : WORD_LEN = 5#usize := by simp [WORD_LEN]

@[step] theorem green_spec : green ⦃ m => m = .Green ⦄ := by simp [green]
@[step] theorem yellow_spec : yellow ⦃ m => m = .Yellow ⦄ := by simp [yellow]

theorem lt_word_len (x : Usize) (h : x < WORD_LEN) : x.val < 5 := by
  rw [word_len_eq] at h; scalar_tac

theorem all_green_total (m : Array Mark 5#usize) : all_green m ⦃ _ => True ⦄ := by
  unfold all_green all_green_loop
  h5i_total (fun i => i) 5
  all_goals (try simp only [word_len_eq] at *)
  all_goals (try scalar_tac)

theorem is_word_total (w : Array U8 5#usize) : is_word w ⦃ _ => True ⦄ := by
  unfold is_word is_word_loop
  h5i_total (fun i => i) 5
  all_goals (try simp only [word_len_eq] at *)
  all_goals (try scalar_tac)

@[step]
theorem find_remaining_bound (r : Array U8 5#usize) (x : U8) :
    find_remaining r x ⦃ o => ∀ j, o = some j → j.val < 5 ⦄ := by
  unfold find_remaining find_remaining_loop
  apply H5iAppLib.loop_idx_spec _ (fun (i : Usize) => i) 5 (fun _ => True) _ ?step _ trivial (by simp)
  intro i _ hi
  unfold find_remaining_loop.body
  split
  · rename_i h; have := lt_word_len i h
    step*
  · simp

theorem mark_greens_total (s g : Array U8 5#usize) : mark_greens s g ⦃ _ => True ⦄ := by
  unfold mark_greens mark_greens_loop
  apply H5iAppLib.loop_idx_spec _ (fun (x : Array Mark 5#usize × Array U8 5#usize × Usize) => x.2.2) 5
    (fun _ => True) _ ?step _ trivial (by simp)
  intro ⟨m, r, i⟩ _ hi
  unfold mark_greens_loop.body
  simp only []
  split
  · rename_i h; have := lt_word_len i h
    step*
  · simp

theorem mark_yellows_total (g : Array U8 5#usize) (m : Array Mark 5#usize) (r : Array U8 5#usize) :
    mark_yellows g m r ⦃ _ => True ⦄ := by
  unfold mark_yellows mark_yellows_loop
  apply H5iAppLib.loop_idx_spec _ (fun (x : Array Mark 5#usize × Array U8 5#usize × Usize) => x.2.2) 5
    (fun _ => True) _ ?step _ trivial (by simp)
  intro ⟨mk, l, i⟩ _ hi
  unfold mark_yellows_loop.body
  simp only []
  split
  · rename_i h; have := lt_word_len i h
    step*
    all_goals try exact mark_eq_spec _ _
  · simp

theorem allM_spec {α : Type} (f : α → Result Bool) (hf : ∀ x, f x ⦃ _ => True ⦄) (l : List α) :
    List.allM f l ⦃ _ => True ⦄ := by
  induction l with
  | nil => simp [List.allM, Std.pure_tc_eq]
  | cons x xs ih =>
    obtain ⟨b, hb, -⟩ := (WP.spec_equiv_exists _ _).1 (hf x)
    simp only [List.allM, hb, Std.bind_tc_eq, Std.bind_ok]
    cases b
    · simp [Std.pure_tc_eq]
    · simpa using ih

@[step]
theorem array_eq_total (a b : Array U8 5#usize) :
    core.array.equality.PartialEqArray.eq core.cmp.PartialEqU8 a b ⦃ _ => True ⦄ := by
  unfold core.array.equality.PartialEqArray.eq
  split
  · apply allM_spec
    intro ⟨x, y⟩
    simp [liftFun2]
  · simp

@[step]
theorem already_guessed_total (game : Core) (w : Array U8 5#usize) : already_guessed game w ⦃ _ => True ⦄ := by
  unfold already_guessed already_guessed_loop
  apply H5iAppLib.loop_idx_spec _ (fun (i : Usize) => i) 6 (fun _ => True) _ ?step _ trivial (by simp)
  intro i _ hi
  unfold already_guessed_loop.body
  step*

@[step]
theorem mark_total (s g : Array U8 5#usize) : wordle_kernel.mark s g ⦃ _ => True ⦄ := by
  unfold wordle_kernel.mark
  have h1 := mark_greens_total s g
  obtain ⟨⟨m, r⟩, hr, -⟩ := (WP.spec_equiv_exists _ _).1 h1
  rw [hr]; simp only [Std.bind_ok]
  exact mark_yellows_total g m r

theorem completion_milli_range (marks : Array Mark 5#usize) :
    completion_milli marks ⦃ r => 0 ≤ r.val ∧ r.val ≤ 1000 ⦄ :=
  WP.spec_mono (completion_milli_spec marks) (fun _ h => ⟨h.2.1, h.2.2⟩)

@[step]
theorem all_green_total' (m : Array Mark 5#usize) : all_green m ⦃ _ => True ⦄ := all_green_total m

@[step]
theorem is_word_total' (w : Array U8 5#usize) : is_word w ⦃ _ => True ⦄ := is_word_total w

@[step]
theorem latest_completion_spec (g : Core) :
    latest_completion g ⦃ r => 0 ≤ r.val ∧ r.val ≤ 1000 ⦄ := by
  unfold latest_completion
  step*

@[step]
theorem invalid_reason_total (g : Core) (mv : Move) : invalid_reason g mv ⦃ _ => True ⦄ := by
  unfold invalid_reason
  cases mv <;> step*

@[step]
theorem letters_of_total (mv : Move) : letters_of mv ⦃ _ => True ⦄ := by
  unfold letters_of; cases mv <;> simp

theorem step_core_shape (g : Core) (mv : Move) (he : g.error_count.val ≤ 1) :
    step_core g mv ⦃ r => ∀ g' ph, r = .Ok (g', ph) →
      g'.secret = g.secret ∧ 0 ≤ g'.reward_milli.val ∧ g'.reward_milli.val ≤ 1000 ∧
      g'.error_count.val ≤ 1 ∧ (g'.done = true ∨ movesLeft g' < movesLeft g) ⦄ := by
  unfold step_core
  simp only [MAX_GUESSES, ERROR_ALLOWANCE, WIN_MILLI]
  step*
  all_goals (intro g' ph h; simp only [core.result.Result.Ok.injEq, Prod.mk.injEq] at h; obtain ⟨rfl, rfl⟩ := h)
  all_goals (try simp only [movesLeft])
  all_goals (try simp_all)
  all_goals (try scalar_tac)

theorem step_core_safe (g : Core) (mv : Move) :
    step_core g mv ⦃ r => ∀ g' ph, r = .Ok (g', ph) →
      g'.secret = g.secret ∧ 0 ≤ g'.reward_milli.val ∧ g'.reward_milli.val ≤ 1000 ⦄ := by
  unfold step_core
  simp only [MAX_GUESSES, ERROR_ALLOWANCE, WIN_MILLI]
  step*
  all_goals (intro g' ph h; simp only [core.result.Result.Ok.injEq, Prod.mk.injEq] at h; obtain ⟨rfl, rfl⟩ := h)
  all_goals (try simp_all)

/-- The transition's step result, from `step_core`'s. -/
theorem transition_step (c : Core) (mv : Move) (P : Core → Reply → Prop)
    (h : step_core c mv ⦃ r => ∀ g' ph, r = .Ok (g', ph) →
      P g' { reward_milli := g'.reward_milli, done := g'.done, phase := some ph } ⦄) :
    transition () c (.Step mv) ⦃ r => ∀ c' rep, r = .Ok (some c', rep) → P c' rep ⦄ := by
  unfold transition
  obtain ⟨r, hr, hp⟩ := (WP.spec_equiv_exists _ _).1 h
  simp only []
  rw [hr]; simp only [Std.bind_ok]
  cases r with
  | Ok p =>
    obtain ⟨g', ph⟩ := p
    show (ok (core.result.Result.Ok (some g',
      ({ reward_milli := g'.reward_milli, done := g'.done, phase := some ph } : Reply)))) ⦃
        r => ∀ c' rep, r = core.result.Result.Ok (some c', rep) → P c' rep ⦄
    simp only [WP.spec_ok]
    intro c' rep he
    simp only [core.result.Result.Ok.injEq, Prod.mk.injEq, Option.some.injEq] at he
    obtain ⟨rfl, rfl⟩ := he
    exact hp g' ph rfl
  | Err e => simp

/-- Every reward a step commits is between 0 and 1000. -/
theorem reward_in_range (c : Core) (mv : Move) :
    transition () c (.Step mv) ⦃ r => ∀ c' rep, r = .Ok (some c', rep) →
      0 ≤ rep.reward_milli.val ∧ rep.reward_milli.val ≤ 1000 ⦄ :=
  transition_step c mv _ (WP.spec_mono (step_core_safe c mv) (fun _ h g' ph he => by
    obtain ⟨-, h1, h2⟩ := h g' ph he; exact ⟨h1, h2⟩))

/-- No move changes the secret. -/
theorem secret_is_fixed (c : Core) (mv : Move) :
    transition () c (.Step mv) ⦃ r => ∀ c' rep, r = .Ok (some c', rep) → c'.secret = c.secret ⦄ :=
  transition_step c mv _ (WP.spec_mono (step_core_safe c mv) (fun _ h g' ph he => (h g' ph he).1))

/-- Each move ends the game or uses up one of `Spec.movesLeft`, and keeps
the invalid-move count at most one. -/
theorem game_ends (c : Core) (mv : Move) (he : c.error_count.val ≤ 1) :
    transition () c (.Step mv) ⦃ r => ∀ c' rep, r = .Ok (some c', rep) →
      c'.error_count.val ≤ 1 ∧ (c'.done = true ∨ movesLeft c' < movesLeft c) ⦄ :=
  transition_step c mv _ (WP.spec_mono (step_core_shape c mv he) (fun _ h g' ph hr => by
    obtain ⟨-, -, -, h4, h5⟩ := h g' ph hr; exact ⟨h4, h5⟩))

/-- A fresh game has twelve moves left and no invalid move spent. -/
theorem fresh_moves (secret : Array U8 5#usize) :
    fresh secret ⦃ c => movesLeft c = 12 ∧ c.error_count.val = 0 ⦄ := by
  unfold fresh; simp [movesLeft]

/-- A finished game refuses every move and commits nothing. -/
theorem done_is_final (c : Core) (mv : Move) (hd : c.done = true) :
    transition () c (.Step mv) = ok (.Err .Done) := by
  unfold transition step_core
  simp [hd]

/-- The kernel never fails: no panic, overflow or bad index, for any input. -/
theorem transition_total (a : Unit) (c : Core) (cmd : Command) :
    ∃ r, transition a c cmd = ok r := by
  have h : transition a c cmd ⦃ _ => True ⦄ := by
    unfold transition
    cases cmd with
    | Reset w => step*; simp [fresh]
    | Step mv =>
      have := step_core_safe c mv
      obtain ⟨r, hr, -⟩ := (WP.spec_equiv_exists _ _).1 this
      simp only []; rw [hr]; simp only [Std.bind_ok]
      cases r with
      | Ok p => obtain ⟨g', ph⟩ := p; simp
      | Err e => simp
    | State => simp
  obtain ⟨r, hr, -⟩ := (WP.spec_equiv_exists _ _).1 h
  exact ⟨r, hr⟩

theorem panic_free : H5iAppLib.Obligation.PanicFree transition :=
  H5iAppLib.Obligation.panicFree_of_total transition_total

def writesOf : Option Core → List Core
  | none => []
  | some c => [c]

theorem read_only_sound : H5iAppLib.Obligation.ReadOnlySound read_only transition writesOf := by
  apply H5iAppLib.ReadOnly.sound_of_spec
  intro a c cmd hro
  cases cmd <;> simp [read_only] at hro
  unfold transition
  simp [H5iAppLib.OnOk, writesOf]

/-- A completion is `Spec.completion` of the marks, between 0 and 1000. -/
theorem completion_spec (marks : Array Mark 5#usize) :
    completion_milli marks ⦃ r => r.val = Spec.completion marks.val ∧ 0 ≤ r.val ∧ r.val ≤ 1000 ⦄ :=
  WP.spec_mono (completion_milli_spec marks) (fun r h => by
    obtain ⟨h1, h2, h3⟩ := h
    refine ⟨?_, h2, h3⟩
    rw [h1, fold_completion]; simp)


theorem fold_weight_lt (l : List Mark) (a : Nat) (h : ∃ m ∈ l, m ≠ .Green) :
    l.foldl (fun acc m => acc + weight m) a < a + 200 * l.length := by
  induction l generalizing a with
  | nil => simp at h
  | cons x xs ih =>
    simp only [List.foldl_cons, List.length_cons]
    by_cases hx : x = .Green
    · subst hx
      have : ∃ m ∈ xs, m ≠ .Green := by
        obtain ⟨m, hm, hne⟩ := h
        rcases List.mem_cons.1 hm with rfl | hm
        · exact absurd rfl hne
        · exact ⟨m, hm, hne⟩
      have := ih (a + weight .Green) this; simp [weight] at *; omega
    · have := fold_bound xs (a + weight x)
      have : weight x ≤ 100 := by cases x <;> simp_all [weight]
      omega

/-- `all_green` says whether every mark is green. -/
@[step]
theorem all_green_spec (m : Array Mark 5#usize) :
    all_green m ⦃ b => b = m.val.all Spec.isGreen ⦄ := by
  unfold all_green all_green_loop
  apply WP.spec_mono (H5iAppLib.loop_search m.val (fun x => !Spec.isGreen x) id (fun _ _ => false) true _ ?_ 0#usize (by simp))
  · intro r hr
    simp only [id] at hr
    have := H5iAppLib.search_all _ _ _ hr
    simpa using this
  · intro j hj
    unfold all_green_loop.body
    split
    · rename_i h; have := lt_word_len j h
      step*
      all_goals try exact mark_eq_spec _ _
      · left; exact ⟨by simpa using this, by rw [← m_post]; cases m <;> simp_all [isGreen], rfl⟩
      · exact ⟨by simpa using this, by rw [← m_post]; cases m <;> simp_all [isGreen], by scalar_tac⟩
    · rename_i h
      have h5 : 5 ≤ j.val := by rw [word_len_eq] at h; scalar_tac
      simp only [WP.spec_ok]; right; exact ⟨by simpa using h5, rfl⟩

theorem step_core_loss (g : Core) (mv : Move) :
    step_core g mv ⦃ r => ∀ g' l m gl x, r = .Ok (g', .Played l m gl (.Loss x)) →
      g'.reward_milli.val < 1000 ⦄ := by
  unfold step_core
  simp only [MAX_GUESSES, ERROR_ALLOWANCE, WIN_MILLI]
  step*
  all_goals (intro g' l m gl x h; simp only [core.result.Result.Ok.injEq, Prod.mk.injEq] at h)
  all_goals (try (obtain ⟨_, h⟩ := h; cases h))
  rename_i hb _ _ _ _ _ heq
  subst heq
  have hne : ∃ x ∈ marks.val, x ≠ .Green := by
    have hf : marks.val.all isGreen = false := by simp_all
    rw [List.all_eq_false] at hf
    obtain ⟨x, hx, hg⟩ := hf
    exact ⟨x, hx, fun e => by subst e; simp [isGreen] at hg⟩
  have := fold_weight_lt marks.val 0 hne
  have hl : marks.val.length = 5 := by simp
  simp_all

/-- A losing guess pays its completion, which is less than 1000: only a win
pays in full. -/
theorem loss_pays_less (c : Core) (mv : Move) :
    transition () c (.Step mv) ⦃ r => ∀ c' rep, r = .Ok (some c', rep) →
      ∀ l m g x, rep.phase = some (.Played l m g (.Loss x)) → rep.reward_milli.val < 1000 ⦄ :=
  transition_step c mv _ (WP.spec_mono (step_core_loss c mv) (fun _ h g' ph he l m gl x hp => by
    simp only [Option.some.injEq] at hp
    subst hp
    exact h g' l m gl x he))

/-! ## The marking rule, wins, and repeats -/

/-- Letters of a word as numbers. -/
def nats (l : List U8) : List Nat := l.map (·.val)

attribute [-step] wordle_kernel.Theorems.find_remaining_bound

/-- `find_remaining` returns the first position holding the letter. -/
@[step]
theorem find_remaining_spec (r : Array U8 5#usize) (x : U8) :
    find_remaining r x ⦃ o => o.map (·.val) = r.val.findIdx? (· = x) ⦄ := by
  unfold find_remaining find_remaining_loop
  apply WP.spec_mono (H5iAppLib.loop_search r.val (fun y => decide (y = x)) (fun o : Option Usize => o.map (·.val))
    (fun i _ => some i) none _ ?_ 0#usize (by simp))
  · intro o ho
    rw [ho, H5iAppLib.searchFrom_findIdx]; simp
  · intro j hj
    unfold find_remaining_loop.body
    split
    · rename_i h; have := lt_word_len j h
      step*
      · left; exact ⟨by simpa using this, by simp_all, rfl⟩
      · exact ⟨by simpa using this, by simp_all, by scalar_tac⟩
    · rename_i h
      have h5 : 5 ≤ j.val := by rw [word_len_eq] at h; scalar_tac
      simp only [WP.spec_ok]; right; exact ⟨by simpa using h5, rfl⟩

theorem consume_of_findIdx (l : List U8) (x : U8) (j : Nat)
    (h : l.findIdx? (· = x) = some j) (hj : j < l.length) :
    nats (l.set j CONSUMED) = Model.consume (nats l) x.val := by
  induction l generalizing j with
  | nil => simp at hj
  | cons y ys ih =>
    rw [List.findIdx?_cons] at h
    by_cases hy : y = x
    · subst hy
      simp at h; subst h
      simp [nats, Model.consume, CONSUMED]
    · have hy' : y.val ≠ x.val := fun e => hy (by scalar_tac)
      simp only [hy, decide_false, Bool.false_eq_true, if_false] at h
      cases hk : ys.findIdx? (· = x) with
      | none => simp [hk] at h
      | some k =>
        simp [hk] at h; subst h
        simp only [List.set_cons_succ, nats, List.map_cons, Model.consume, hy', if_false]
        congr 1
        exact ih k hk (by simpa using hj)

theorem not_mem_of_findIdx (l : List U8) (x : U8) (h : l.findIdx? (· = x) = none) :
    x.val ∉ nats l := by
  rw [List.findIdx?_eq_none_iff] at h
  simp only [nats, List.mem_map, not_exists, not_and]
  intro y hy hyx
  have := h y hy
  simp at this
  exact this (by scalar_tac)

theorem mem_of_findIdx (l : List U8) (x : U8) (j : Nat) (h : l.findIdx? (· = x) = some j) :
    x.val ∈ nats l := by
  have := List.findIdx?_eq_some_iff_getElem.1 h
  obtain ⟨hj, hp, -⟩ := this
  simp only [nats, List.mem_map]
  exact ⟨l[j], List.getElem_mem hj, by simp at hp; rw [hp]⟩

theorem find_some_lt (l : Array U8 5#usize) (x : U8) (o : Option Usize) {j : Usize}
    (h : o.map (fun x => x.val) = l.val.findIdx? (· = x)) (ho : o = some j) : j.val < l.val.length := by
  subst ho
  exact (List.findIdx?_eq_some_iff_getElem.1 (by simpa using h.symm)).1

/-- What the rest of the yellow pass will produce from a loop state. -/
def yellowsFrom (g : List Nat) (x : Array Mark 5#usize × Array U8 5#usize × Usize) : List Mark :=
  x.1.val.take x.2.2.val ++ Model.yellows (g.drop x.2.2.val) (x.1.val.drop x.2.2.val) (nats x.2.1.val)

theorem take_set_succ {α} (l : List α) (i : Nat) (v : α) (hi : i < l.length) :
    (l.set i v).take (i + 1) = l.take i ++ [v] := by
  rw [List.take_add_one, List.take_set, List.set_eq_of_length_le (by simp)]
  simp [hi]

theorem drop_set_succ {α} (l : List α) (i : Nat) (v : α) :
    (l.set i v).drop (i + 1) = l.drop (i + 1) := by
  simp [List.drop_set]

theorem take_succ' {α} (l : List α) (i : Nat) (hi : i < l.length) : l.take (i + 1) = l.take i ++ [l[i]] := by
  rw [List.take_add_one, List.getElem?_eq_getElem hi]; simp

theorem drop_cons {α} (l : List α) (i : Nat) (hi : i < l.length) : l.drop i = l[i] :: l.drop (i + 1) :=
  List.drop_eq_getElem_cons hi

theorem yellowsFrom_green (g : List Nat) (mk : Array Mark 5#usize) (l : Array U8 5#usize) (i i2 : Usize)
    (hi : i.val < 5) (hg : g.length = 5) (hi2 : i2.val = i.val + 1) (hm : mk.val[i.val]'(by simp; omega) = .Green) :
    yellowsFrom g (mk, l, i2) = yellowsFrom g (mk, l, i) := by
  simp only [yellowsFrom, hi2]
  rw [drop_cons g i.val (by omega), drop_cons mk.val i.val (by simp; omega), List.take_add_one]
  simp [hm, Model.yellows, isGreen, List.getElem?_eq_getElem (by simp; omega : i.val < mk.val.length)]

theorem yellowsFrom_none (g : List Nat) (mk : Array Mark 5#usize) (l : Array U8 5#usize) (i i2 : Usize)
    (hi : i.val < 5) (hg : g.length = 5) (hi2 : i2.val = i.val + 1)
    (hm : mk.val[i.val]'(by simp; omega) ≠ .Green) (hn : g[i.val]'(by omega) ∉ nats l.val) :
    yellowsFrom g (mk, l, i2) = yellowsFrom g (mk, l, i) := by
  simp only [yellowsFrom, hi2]
  rw [drop_cons g i.val (by omega), drop_cons mk.val i.val (by simp; omega)]
  have hng : isGreen (mk.val[i.val]'(by simp; omega)) = false := by
    cases h : mk.val[i.val]'(by simp; omega) <;> simp_all [isGreen]
  simp only [Model.yellows, hng, hn, if_false, Bool.false_eq_true]
  rw [take_succ' mk.val i.val (by simp; omega)]
  simp only [List.append_assoc, List.singleton_append]

theorem yellowsFrom_some (g : List Nat) (mk : Array Mark 5#usize) (l : Array U8 5#usize) (i i2 : Usize)
    (j : Nat) (j' : Usize) (hjj : j'.val = j) (hi : i.val < 5) (hg : g.length = 5) (hi2 : i2.val = i.val + 1)
    (hm : mk.val[i.val]'(by simp; omega) ≠ .Green)
    (hin : g[i.val]'(by omega) ∈ nats l.val)
    (hc : nats (l.val.set j CONSUMED) = Model.consume (nats l.val) (g[i.val]'(by omega))) :
    yellowsFrom g (mk.set i .Yellow, l.set j' CONSUMED, i2) = yellowsFrom g (mk, l, i) := by
  have hng : isGreen (mk.val[i.val]'(by simp; omega)) = false := by
    cases h : mk.val[i.val]'(by simp; omega) <;> simp_all [isGreen]
  simp only [yellowsFrom, hi2]
  rw [drop_cons g i.val (by omega), drop_cons mk.val i.val (by simp; omega)]
  simp only [Model.yellows, hng, hin, if_true, if_false, Bool.false_eq_true]
  have hset : (mk.set i .Yellow).val = mk.val.set i.val .Yellow := by simp
  have hset2 : (l.set j' CONSUMED).val = l.val.set j CONSUMED := by simp [hjj]
  rw [hset, hset2, take_set_succ _ _ _ (by simp; omega), drop_set_succ]
  simp only [nats] at hc ⊢
  rw [hc]
  simp only [List.append_assoc, List.singleton_append]
@[step]
theorem mark_yellows_spec (g : Array U8 5#usize) (m : Array Mark 5#usize) (r : Array U8 5#usize) :
    mark_yellows g m r ⦃ out => out.val = Model.yellows (nats g.val) m.val (nats r.val) ⦄ := by
  unfold mark_yellows mark_yellows_loop
  apply loop.spec_decr_nat (measure := fun (x : Array Mark 5#usize × Array U8 5#usize × Usize) => 5 - x.2.2.val)
    (inv := fun x => x.2.2.val ≤ 5 ∧ yellowsFrom (nats g.val) x = yellowsFrom (nats g.val) (m, r, 0#usize))
  · rintro ⟨mk, l, i⟩ ⟨hi, heq⟩
    simp only at hi heq
    unfold mark_yellows_loop.body
    simp only []
    split
    · rename_i h; have hi5 := lt_word_len i h
      have hl : (mk.val).length = 5 := by simp
      have hgl : (nats g.val).length = 5 := by simp [nats]
      step*
      all_goals try exact mark_eq_spec _ _
      all_goals try (apply find_some_lt _ _ _ o_post; assumption)
      · -- not green, no copy left
        rename_i hno
        subst hno
        have hn := not_mem_of_findIdx l.val i1 (by simpa using o_post.symm)
        have hgi : (nats g.val)[i.val]'(by omega) = i1.val := by simp [nats, i1_post]
        refine ⟨by omega, ?_, by omega⟩
        rw [yellowsFrom_none _ mk l i i2 hi5 hgl i2_post (by rw [← m_post]; exact b_post.1 ‹b = true›) (by rw [hgi]; exact hn), heq]
      · -- not green, a copy at j
        have hnotg : mk.val[i.val]'(by simp; omega) ≠ .Green := by rw [← m_post]; exact b_post.1 ‹b = true›
        subst (‹o = some j› : o = some j)
        have hf : l.val.findIdx? (· = i1) = some j.val := by simpa using o_post.symm
        have hjl : j.val < l.val.length := (List.findIdx?_eq_some_iff_getElem.1 hf).1
        have hgi : (nats g.val)[i.val]'(by omega) = i1.val := by simp [nats, i1_post]
        refine ⟨by omega, ?_, by omega⟩
        subst a_post a1_post m1_post
        rw [yellowsFrom_some _ mk l i i2 j.val j rfl hi5 hgl i2_post hnotg
          (by rw [hgi]; exact mem_of_findIdx _ _ _ hf)
          (by rw [hgi]; exact consume_of_findIdx _ _ _ hf hjl), heq]
      · -- green
        have hg : mk.val[i.val]'(by simp; omega) = .Green := by
          rw [← m_post]; by_contra hne; exact (by assumption : ¬ b = true) (b_post.2 hne)
        refine ⟨by omega, ?_, by omega⟩
        rw [yellowsFrom_green _ mk l i i2 hi5 hgl i2_post hg, heq]
    · rename_i h
      have h5 : i.val = 5 := by rw [word_len_eq] at h; scalar_tac
      simp only [WP.spec_ok]
      simp only [yellowsFrom, h5] at heq
      have hd : (nats g.val).drop 5 = [] := by simp [nats]
      have hm : mk.val.drop 5 = [] := by simp
      have ht : mk.val.take 5 = mk.val := by simp
      rw [hd, hm, ht] at heq
      simpa [Model.yellows] using heq
  · refine ⟨by simp, rfl⟩

/-- The green-pass loop invariant at state `x`. -/
def GreensInv (S G : List Nat) (x : Array Mark 5#usize × Array U8 5#usize × Usize) : Prop :=
  x.2.2.val ≤ 5 ∧
  x.1.val.take x.2.2.val ++ Model.greens (S.drop x.2.2.val) (G.drop x.2.2.val) = Model.greens S G ∧
  (nats x.2.1.val).take x.2.2.val ++ Model.left (S.drop x.2.2.val) (G.drop x.2.2.val) = Model.left S G ∧
  nats (x.2.1.val.drop x.2.2.val) = S.drop x.2.2.val ∧
  x.1.val.drop x.2.2.val = List.replicate (5 - x.2.2.val) .Wrong

theorem greens_cons (s g : Nat) (ss gs : List Nat) :
    Model.greens (s :: ss) (g :: gs) = (if g = s then .Green else .Wrong) :: Model.greens ss gs := rfl

theorem left_cons (s g : Nat) (ss gs : List Nat) :
    Model.left (s :: ss) (g :: gs) = (if g = s then Model.used else s) :: Model.left ss gs := rfl

theorem greens_inv_step (S G : List Nat) (hS : S.length = 5) (hG : G.length = 5)
    (mk mk' : Array Mark 5#usize) (rm rm' : Array U8 5#usize) (i i' : Usize) (hi : i.val < 5)
    (hi' : i'.val = i.val + 1)
    (inv : GreensInv S G (mk, rm, i))
    (hmk : mk'.val = mk.val.set i.val (if G[i.val]'(by omega) = S[i.val]'(by omega) then .Green else .Wrong))
    (hrm : nats rm'.val = (nats rm.val).set i.val
      (if G[i.val]'(by omega) = S[i.val]'(by omega) then Model.used else S[i.val]'(by omega))) :
    GreensInv S G (mk', rm', i') := by
  obtain ⟨-, hg, hl, hd, hw⟩ := inv
  simp only at hg hl hd hw ⊢
  have hml : mk.val.length = 5 := by simp
  have hrl : (nats rm.val).length = 5 := by simp [nats]
  rw [drop_cons S i.val (by omega), drop_cons G i.val (by omega), greens_cons] at hg
  rw [drop_cons S i.val (by omega), drop_cons G i.val (by omega), left_cons] at hl
  unfold GreensInv
  simp only []
  rw [hi']
  refine ⟨by omega, ?_, ?_, ?_, ?_⟩
  · rw [hmk, take_set_succ _ _ _ (by omega), ← hg]; simp
  · rw [hrm, take_set_succ _ _ _ (by omega), ← hl]; simp
  · have : nats (rm'.val.drop (i.val + 1)) = (nats rm'.val).drop (i.val + 1) := by simp [nats, List.map_drop]
    rw [this, hrm, drop_set_succ]
    have h2 : nats (rm.val.drop i.val) = (nats rm.val).drop i.val := by simp [nats, List.map_drop]
    rw [h2, drop_cons _ i.val (by omega), drop_cons S i.val (by omega)] at hd
    exact (List.cons.inj hd).2
  · rw [hmk, drop_set_succ]
    rw [drop_cons _ i.val (by omega)] at hw
    have : 5 - i.val = (5 - (i.val + 1)) + 1 := by omega
    rw [this, List.replicate_succ] at hw
    exact (List.cons.inj hw).2

@[step]
theorem mark_greens_spec (s g : Array U8 5#usize) :
    mark_greens s g ⦃ out => out.1.val = Model.greens (nats s.val) (nats g.val) ∧
      nats out.2.val = Model.left (nats s.val) (nats g.val) ⦄ := by
  unfold mark_greens mark_greens_loop
  apply loop.spec_decr_nat (measure := fun (x : Array Mark 5#usize × Array U8 5#usize × Usize) => 5 - x.2.2.val)
    (inv := GreensInv (nats s.val) (nats g.val))
  · rintro ⟨mk, rm, i⟩ ⟨hi, hg, hl, hd, hw⟩
    simp only at hi hg hl hd hw
    unfold mark_greens_loop.body
    simp only []
    split
    · rename_i h; have hi5 := lt_word_len i h
      have hS : (nats s.val).length = 5 := by simp [nats]
      have hG : (nats g.val).length = 5 := by simp [nats]
      have inv : GreensInv (nats s.val) (nats g.val) (mk, rm, i) := ⟨hi, hg, hl, hd, hw⟩
      -- The secret letter at `i` is still in `rm`, and the mark there is still `Wrong`.
      have hdi : (nats rm.val)[i.val]'(by simp [nats]; omega) = (nats s.val)[i.val]'(by omega) := by
        have h2 : nats (rm.val.drop i.val) = (nats rm.val).drop i.val := by simp [nats, List.map_drop]
        rw [h2, drop_cons _ i.val (by simp [nats]; omega), drop_cons _ i.val (by omega)] at hd
        exact (List.cons.inj hd).1
      have hwi : mk.val[i.val]'(by simp; omega) = .Wrong := by
        rw [drop_cons _ i.val (by simp; omega)] at hw
        have : 5 - i.val = (5 - (i.val + 1)) + 1 := by omega
        rw [this, List.replicate_succ] at hw
        exact (List.cons.inj hw).1
      have hgi : (nats g.val)[i.val]'(by omega) = (g.val[i.val]'(by simp; omega)).val := by simp [nats]
      have hri : (nats rm.val)[i.val]'(by simp [nats]; omega) = (rm.val[i.val]'(by simp; omega)).val := by simp [nats]
      step*
      · -- the guess letter matches: green, and the secret letter is used up
        have heqv : (nats g.val)[i.val]'(by omega) = (nats s.val)[i.val]'(by omega) := by
          rw [hgi, ← hdi, hri]; simp_all
        refine ⟨greens_inv_step _ _ hS hG mk _ rm _ i _ hi5 (by scalar_tac) inv ?_ ?_, by scalar_tac⟩
        · simp_all
        · have heqv' : (g.val[i.val]'(by simp; omega)).val = (s.val[i.val]'(by simp; omega)).val := by
            simpa [nats] using heqv
          simp [nats, heqv', CONSUMED, List.map_set, a1_post, Model.used]
      · -- no match: nothing changes
        have hnev : ¬ (nats g.val)[i.val]'(by omega) = (nats s.val)[i.val]'(by omega) := by
          rw [hgi, ← hdi, hri]; intro he; apply ‹¬ _›; simp_all; try scalar_tac
        refine ⟨greens_inv_step _ _ hS hG mk mk rm rm i _ hi5 (by scalar_tac) inv ?_ ?_, by scalar_tac⟩
        · rw [if_neg hnev, ← hwi, List.set_getElem_self]
        · rw [if_neg hnev, ← hdi, List.set_getElem_self]
    · rename_i h
      have h5 : i.val = 5 := by rw [word_len_eq] at h; scalar_tac
      simp only [WP.spec_ok]
      rw [h5] at hg hl
      have e1 : (nats s.val).drop 5 = [] := by simp [nats]
      have e2 : (nats g.val).drop 5 = [] := by simp [nats]
      have e3 : mk.val.take 5 = mk.val := by simp
      have e4 : (nats rm.val).take 5 = nats rm.val := by simp [nats]
      rw [e1, e2, e3] at hg; rw [e1, e2, e4] at hl
      simp only [Model.greens, Model.left, List.append_nil] at hg hl
      exact ⟨hg, hl⟩
  · refine ⟨by simp, by simp, by simp [nats], by simp [nats], ?_⟩
    simp [Array.repeat]

/-- For words of letters below 26, the extracted `mark` is the TextArena rule. -/
@[step]
theorem mark_spec (secret guess : Array U8 5#usize)
    (hs : ∀ x ∈ secret.val, x.val < 26) (hg : ∀ x ∈ guess.val, x.val < 26) :
    wordle_kernel.mark secret guess ⦃ m => m.val = Spec.mark (word secret) (word guess) ⦄ := by
  unfold wordle_kernel.mark
  step*
  rw [m_post, greens_post, greens_post1]
  have hw : ∀ w : Array U8 5#usize, word w = nats w.val := fun w => rfl
  rw [hw, hw]
  apply Model.mark_eq_spec
  · simp [nats]
  · intro x hx; simp only [nats, List.mem_map] at hx; obtain ⟨y, hy, rfl⟩ := hx; exact hs y hy
  · intro x hx; simp only [nats, List.mem_map] at hx; obtain ⟨y, hy, rfl⟩ := hx; exact hg y hy

theorem markPairs_all_green (pairs : List (Nat × Nat)) (rem : Nat → Nat) :
    (markPairs pairs rem).all isGreen = pairs.all (fun p => decide (p.1 = p.2)) := by
  induction pairs generalizing rem with
  | nil => simp [markPairs]
  | cons p ps ih =>
    obtain ⟨a, b⟩ := p
    simp only [markPairs]
    by_cases h : a = b
    · simp [h, isGreen, ih]
    · rw [if_neg h]
      split <;> simp [h, isGreen, List.all_cons]

theorem all_pairs_eq (s g : List Nat) (hl : s.length = g.length) :
    (s.zip g).all (fun p => decide (p.1 = p.2)) = true ↔ s = g := by
  induction s generalizing g with
  | nil => cases g <;> simp_all
  | cons a rest ih =>
    cases g with
    | nil => simp at hl
    | cons b bs =>
      simp only [List.zip_cons_cons, List.all_cons, Bool.and_eq_true, decide_eq_true_eq, List.cons.injEq]
      rw [ih bs (by simpa using hl)]

/-- All five marks are green exactly when the guess is the secret. -/
theorem spec_all_green_iff (s g : List Nat) (hl : s.length = g.length) :
    (Spec.mark s g).all isGreen = true ↔ g = s := by
  rw [Spec.mark, markPairs_all_green, all_pairs_eq s g hl]
  exact ⟨fun h => h.symm, fun h => h.symm⟩

@[step]
theorem is_word_spec (w : Array U8 5#usize) :
    is_word w ⦃ b => b = true ↔ ∀ x ∈ w.val, x.val < 26 ⦄ := by
  unfold is_word is_word_loop
  apply WP.spec_mono (H5iAppLib.loop_search w.val (fun x => decide (26 ≤ x.val)) id (fun _ _ => false) true _ ?_ 0#usize (by simp))
  · intro r hr
    simp only [id] at hr
    have := H5iAppLib.search_all _ _ _ hr
    rw [this]
    simp only [List.all_eq_true, Bool.not_eq_true', decide_eq_false_iff_not, Nat.not_le]
  · intro j hj
    unfold is_word_loop.body
    split
    · rename_i h; have := lt_word_len j h
      step*
      · left; exact ⟨by simpa using this, by simp_all, rfl⟩
      · exact ⟨by simpa using this, by simp_all, by scalar_tac⟩
    · rename_i h
      have h5 : 5 ≤ j.val := by rw [word_len_eq] at h; scalar_tac
      simp only [WP.spec_ok]; right; exact ⟨by simpa using h5, rfl⟩

@[step]
theorem invalid_reason_spec (g : Core) (mv : Move) :
    invalid_reason g mv ⦃ o => o = none →
      ∃ letters, mv = .Candidate letters true ∧ ∀ x ∈ letters.val, x.val < 26 ⦄ := by
  unfold invalid_reason
  cases mv with
  | Candidate letters dict =>
    step*
  | Malformed r => simp

attribute [-step] wordle_kernel.Theorems.letters_of_total

@[step]
theorem letters_of_spec (mv : Move) :
    letters_of mv ⦃ l => ∀ ls d, mv = .Candidate ls d → l = ls ⦄ := by
  unfold letters_of; cases mv <;> simp

theorem step_core_played (g : Core) (mv : Move) (hs : ∀ x ∈ g.secret.val, x.val < 26) :
    step_core g mv ⦃ r => ∀ g' l m gl o, r = .Ok (g', .Played l m gl o) →
      m.val = Spec.mark (word g.secret) (word l) ∧
      (o = .Win ↔ word l = word g.secret) ∧
      (o = .Win → g'.reward_milli.val = 1000) ∧ (o = .Ongoing → g'.reward_milli.val = 0) ⦄ := by
  unfold step_core
  simp only [MAX_GUESSES, ERROR_ALLOWANCE, WIN_MILLI]
  step*
  -- The guess is a word of letters below 26: `invalid_reason` returned `none`.
  all_goals try (
    intro x hx
    obtain ⟨ls, hmv, hls⟩ := o_post ‹_›
    rw [letters_post ls true hmv] at hx
    exact hls x hx)
  all_goals (intro g' l m gl o h; simp only [core.result.Result.Ok.injEq, Prod.mk.injEq, Phase.Played.injEq] at h)
  all_goals (obtain ⟨hg', rfl, rfl, -, rfl⟩ := h; subst hg')
  all_goals (have hlen : (word g.secret).length = (word letters).length := by simp [word])
  all_goals (have hiff := spec_all_green_iff _ _ hlen; rw [← marks_post] at hiff; rw [← b_post] at hiff)
  all_goals (refine ⟨marks_post, ?_, ?_, ?_⟩)
  all_goals simp_all


/-- `Reset` keeps only secrets whose letters are below 26. -/
theorem reset_secret_valid (c : Core) (w : Array U8 5#usize) :
    transition () c (.Reset w) ⦃ r => ∀ c' rep, r = .Ok (some c', rep) → ∀ x ∈ c'.secret.val, x.val < 26 ⦄ := by
  unfold transition
  step*
  simp only [fresh, Std.bind_tc_ok, Std.bind_ok, WP.spec_ok]
  intro c' rep h
  simp only [core.result.Result.Ok.injEq, Prod.mk.injEq, Option.some.injEq] at h
  obtain ⟨rfl, -⟩ := h
  exact b_post.1 ‹_›

/-- A scored guess is marked by the TextArena rule, and it pays 1000 exactly
when it is the secret. -/
theorem guess_pays_iff_secret (c : Core) (mv : Move) (hs : ∀ x ∈ c.secret.val, x.val < 26) :
    transition () c (.Step mv) ⦃ r => ∀ c' rep, r = .Ok (some c', rep) →
      ∀ l m gl o, rep.phase = some (.Played l m gl o) →
        m.val = Spec.mark (word c.secret) (word l) ∧
        (rep.reward_milli.val = 1000 ↔ word l = word c.secret) ⦄ := by
  have hp := step_core_played c mv hs
  have hl := step_core_loss c mv
  have h2 : step_core c mv ⦃ r => ∀ g' ph, r = .Ok (g', ph) → ∀ l m gl o, some ph = some (Phase.Played l m gl o) →
      m.val = Spec.mark (word c.secret) (word l) ∧ (g'.reward_milli.val = 1000 ↔ word l = word c.secret) ⦄ := by
    obtain ⟨r, hr, hp'⟩ := (WP.spec_equiv_exists _ _).1 hp
    have hl' := post_of_ok hl hr
    rw [hr]; simp only [WP.spec_ok]
    intro g' ph he l m gl o hph
    simp only [Option.some.injEq] at hph; subst hph
    obtain ⟨h1, h2, h3, h4⟩ := hp' g' l m gl o he
    refine ⟨h1, ?_⟩
    cases o with
    | Win => simp_all
    | Ongoing => simp_all
    | Loss x =>
      have := hl' g' l m gl x he
      constructor
      · intro h; omega
      · intro h; have := h2.2 h; cases this
  exact transition_step c mv _ (WP.spec_mono h2 (fun _ h g' ph he l m gl o hph => h g' ph he l m gl o hph))

theorem allM_pure {α : Type} (p : α → Bool) (l : List α) :
    List.allM (fun x => (ok (p x) : Result Bool)) l = ok (l.all p) := by
  induction l with
  | nil => simp [List.allM, Std.pure_tc_eq]
  | cons x xs ih =>
    simp only [List.allM, Std.bind_tc_eq, Std.bind_ok, List.all_cons]
    cases p x <;> simp [ih, Std.pure_tc_eq]

theorem zip_all_eq_u8 (a b : List U8) (hl : a.length = b.length) :
    (a.zip b).all (fun p => decide (p.1 = p.2)) = true ↔ a = b := by
  induction a generalizing b with
  | nil => cases b <;> simp_all
  | cons x xs ih =>
    cases b with
    | nil => simp at hl
    | cons y ys =>
      simp only [List.zip_cons_cons, List.all_cons, Bool.and_eq_true, decide_eq_true_eq, List.cons.injEq]
      rw [ih ys (by simpa using hl)]

@[step]
theorem array_eq_spec (a b : Array U8 5#usize) :
    core.array.equality.PartialEqArray.eq core.cmp.PartialEqU8 a b ⦃ r => r = true ↔ a = b ⦄ := by
  unfold core.array.equality.PartialEqArray.eq
  rw [if_pos (by simp)]
  simp only [liftFun2, Std.bind_tc_eq, Std.bind_ok]
  simp only [allM_pure (fun x : U8 × U8 => decide ¬core.cmp.impls.PartialEqU8.ne x.1 x.2 = true)]
  simp only [WP.spec_ok]
  have hne : ∀ x y : U8, (decide ¬core.cmp.impls.PartialEqU8.ne x y = true) = decide (x = y) := by
    intro x y; simp [core.cmp.impls.PartialEqU8.ne]
    exact ⟨fun h => by scalar_tac, fun h => by rw [h]⟩
  simp only [hne]
  rw [zip_all_eq_u8 _ _ (by simp)]
  constructor
  · intro h; exact Std.Array.ext a b h
  · intro h; rw [h]

attribute [-step] wordle_kernel.Theorems.already_guessed_total

theorem cast_u8_usize (x : U8) : (UScalar.cast UScalarTy.Usize x).val = x.val := by
  simp [UScalar.cast_val_eq]
  apply Nat.mod_eq_of_lt
  have : x.val < 256 := by scalar_tac
  rcases System.Platform.numBits_eq with h | h <;> rw [h] <;> omega

@[step]
theorem already_guessed_spec (g : Core) (w : Array U8 5#usize) :
    already_guessed g w ⦃ b => b = (g.guesses.val.take g.guess_count.val).any (fun a => decide (a = w)) ⦄ := by
  unfold already_guessed already_guessed_loop
  apply WP.spec_mono (H5iAppLib.loop_search (g.guesses.val.take g.guess_count.val) (fun a => decide (a = w)) id
    (fun _ _ => true) false _ ?_ 0#usize (by simp))
  · intro r hr
    simp only [id] at hr
    exact H5iAppLib.search_any _ _ _ hr
  · intro j hj
    unfold already_guessed_loop.body
    step*
    all_goals (have hlen : (List.take g.guess_count.val g.guesses.val).length = min g.guess_count.val 6 := by simp)
    all_goals (have hc : i1.val = g.guess_count.val := by rw [i1_post]; exact cast_u8_usize _)
    · left; refine ⟨by rw [hlen]; scalar_tac, ?_, rfl⟩
      simp only [List.getElem_take]; simp_all
    · refine ⟨by rw [hlen]; scalar_tac, ?_, by scalar_tac⟩
      simp only [List.getElem_take]; simp_all
    · right; exact ⟨by rw [hlen]; scalar_tac, rfl⟩
    · right; exact ⟨by rw [hlen]; scalar_tac, rfl⟩

theorem invalid_reason_fresh (g : Core) (mv : Move) :
    invalid_reason g mv ⦃ o => o = none → ∀ letters d, mv = .Candidate letters d →
      (g.guesses.val.take g.guess_count.val).any (fun a => decide (a = letters)) = false ⦄ := by
  unfold invalid_reason
  cases mv with
  | Candidate letters dict =>
    step*
    all_goals (intro h ls d hmv; simp only [Move.Candidate.injEq] at hmv; obtain ⟨rfl, rfl⟩ := hmv)
    all_goals simp_all
  | Malformed r => simp

/-- A guess equal to an earlier guess of the game is never scored: the move
is refused as invalid instead of being marked. -/
theorem repeat_is_not_scored (g : Core) (letters) (d : Bool)
    (hrep : letters ∈ g.guesses.val.take g.guess_count.val) :
    step_core g (.Candidate letters d) ⦃ r => ∀ g' l m gl o, r = .Ok (g', .Played l m gl o) → False ⦄ := by
  have hany : (g.guesses.val.take g.guess_count.val).any (fun a => decide (a = letters)) = true := by
    rw [List.any_eq_true]; exact ⟨letters, hrep, by simp⟩
  have hi := invalid_reason_fresh g (.Candidate letters d)
  obtain ⟨o, ho, hpost⟩ := (WP.spec_equiv_exists _ _).1 hi
  unfold step_core
  simp only [MAX_GUESSES, ERROR_ALLOWANCE, WIN_MILLI, ho, Std.bind_ok]
  cases o with
  | none => have := hpost rfl letters d rfl; rw [hany] at this; cases this
  | some reason =>
    split <;> try simp
    split <;> try simp
    split <;> try simp
    split <;> step* <;> simp

end wordle_kernel.Theorems
