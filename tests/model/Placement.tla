----------------------------- MODULE Placement -----------------------------
EXTENDS Naturals, FiniteSets, TLC

\* One file moves between two parents with independently changing placement
\* policies. Each policy names one local target. Deletion detaches the file
\* into trash and captures its inherited policy; it DOES NOT delete copies.
\* Restore can choose a different parent. Policy changes and directory moves
\* need not immediately migrate existing copies; only a new publication must
\* use the effective policy current at that publication's linearization point.
\*
\* core/reconcile.rs::reconcile_one copies without the namespace lock, then
\* holds the file and namespace locks to recheck policy(id) and live(id)
\* before atomically publishing. catalog.rs rename/unlink and trash restore
\* are namespace transitions; catalog inheritance uses a coherent DB snapshot.
\* A policy ABA is intentionally allowed: equal effective policies suffice.
\* File generations/verification are checked in Protection, not repeated here.
\* This models local publication only. Archive work may finish for a trashed
\* file under its captured policy. Physical GC and permanent purge are absent.
CONSTANTS Parents, Targets, CheckPolicy, CheckLive
ASSUME Cardinality(Parents) = 2 /\ Cardinality(Targets) = 2
       /\ "trash" \notin Parents /\ "none" \notin Targets
VARIABLES location, policies, detachedPolicy, candidate, copies,
          publishedPolicyOK, publishedLive
vars == <<location, policies, detachedPolicy, candidate, copies,
          publishedPolicyOK, publishedLive>>
Live == location \in Parents
EffectivePolicy == IF Live THEN policies[location] ELSE detachedPolicy

Init ==
    /\ location \in Parents /\ policies \in [Parents -> Targets]
    /\ detachedPolicy \in Targets
    /\ candidate = "none" /\ copies = {policies[location]}
    /\ publishedPolicyOK = TRUE /\ publishedLive = TRUE

Move(parent) ==
    /\ Live /\ location' = parent
    /\ UNCHANGED <<policies, detachedPolicy, candidate, copies,
                   publishedPolicyOK, publishedLive>>
SetPolicy(parent, target) ==
    /\ policies' = [policies EXCEPT ![parent] = target]
    /\ UNCHANGED <<location, detachedPolicy, candidate, copies,
                   publishedPolicyOK, publishedLive>>
Trash ==
    /\ Live /\ detachedPolicy' = EffectivePolicy /\ location' = "trash"
    /\ UNCHANGED <<policies, candidate, copies, publishedPolicyOK, publishedLive>>
Restore(parent) ==
    /\ ~Live /\ location' = parent
    /\ UNCHANGED <<policies, detachedPolicy, candidate, copies,
                   publishedPolicyOK, publishedLive>>
Start ==
    /\ Live /\ candidate = "none" /\ candidate' = EffectivePolicy
    /\ UNCHANGED <<location, policies, detachedPolicy, copies,
                   publishedPolicyOK, publishedLive>>
Publish ==
    /\ candidate # "none"
    /\ (~CheckPolicy \/ candidate = EffectivePolicy)
    /\ (~CheckLive \/ Live)
    /\ copies' = copies \cup {candidate} /\ candidate' = "none"
    /\ publishedPolicyOK' = (candidate = EffectivePolicy)
    /\ publishedLive' = Live
    /\ UNCHANGED <<location, policies, detachedPolicy>>
Abort ==
    /\ candidate # "none" /\ candidate' = "none"
    /\ UNCHANGED <<location, policies, detachedPolicy, copies,
                   publishedPolicyOK, publishedLive>>
Next == (\E p \in Parents : Move(p) \/ Restore(p))
        \/ (\E p \in Parents, t \in Targets : SetPolicy(p, t))
        \/ Trash \/ Start \/ Publish \/ Abort
TypeOK ==
    /\ location \in Parents \cup {"trash"}
    /\ policies \in [Parents -> Targets] /\ detachedPolicy \in Targets
    /\ candidate \in Targets \cup {"none"} /\ copies \subseteq Targets
    /\ publishedPolicyOK \in BOOLEAN /\ publishedLive \in BOOLEAN
PublishesCurrentPolicy == publishedPolicyOK
PublishesLiveNode == publishedLive
RetainsExistingProtection == copies # {}
Spec == Init /\ [][Next]_vars
=============================================================================
