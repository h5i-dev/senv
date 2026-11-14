import Spec
import H5iAppLib
import H5iAppLib.ReadOnly
/-!
# Proofs about the extracted Wordle-v0 kernel

Each theorem is about `generated/WordleKernel.lean`, which `h5i app extract`
produces from `kernel/src/lib.rs`:

* a completion score is `Spec.completion` of the marks, between 0 and 1000,
  and every reward a move commits is between 0 and 1000;
* every game ends: each move either ends it or uses up one of
  `Spec.movesLeft`, which starts at 12, so no game lasts more than twelve moves;
* a finished game refuses every move, and no move changes the secret;
* the kernel never panics, and `State` writes nothing.

Not yet proved here: that `mark` is the TextArena rule of `Spec.mark`, and
that a guess wins exactly when it is the secret. The Rust tests check both.
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

end wordle_kernel.Theorems
