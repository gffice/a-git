# arti dirauth design sketch

## dirauth functions

 * receive relays' server descriptor submissions (and extra-info docs submissions)
 * exchange server submissions with other dirauths
   (acting as a full normal dircache is one way to do this,
   but perhaps a more limited form of dircache is sufficient).
 * participate in the shared random protocol
 * perform some reachability tests for candidate relays
   - includes tracking reachability over time
     (and thus computing Stable and Guard flags)
 * generate a vote from
   - available descriptors
   - configuration (including relay-specific configuration provided
     by network health team, mediated by dirauth local policy
   - bandwidth measurements
 * exchange votes (and consensus signatures) with other dirauths (make them publicly available)
 * given votes, generate and sign consensus
 * serve the consensus document

## principal components

 * dircache.
   (Also needed for Arti Relay; collaborate with that team)
    - store information (eg descriptors, consensuses) locally
    - serve over BEGIN_DIR
    - serve over HTTP
    - Download information as needed; see also
      https://spec.torproject.org/dir-spec/downloading-from-other-auths.html

 * reachability tester and relay status history
   - test relays' reachability
   - record enough history to calculate consensus uptime and MTBF measures

 * ingesters for relay-specific information
   - relay-specific configuration from Network Health
   - bandwidth scanner results

 * consensus algorithm implementation
   - We will not attempt to 100% match the behaviour of C Tor.
     Instead, we provide this as `.so` (or a maybe an executable)
     and will arrange for C Tor diaruth to  be able to use it
     (see transition plan).

 * vote calculator

 * component for generating K\_dirauth\_sign\_* and signing it with
   KS\_dirauth\_id\_*, capable of running offline.

The latter two don't need to be always-online.
We'll to separate them out so that they can (likely in the future)
use a static data dump, or a restricted protocol,
so that they don't need full internet access.

## Computing votes

See also:
 - Everything in the spec under "Directory authority operation and formats"
   through "serving bandwidth list files."
 - The C tor manpage.
 - The files dirvote.c and voteflags.c in C tor.
 - The akashic records as preserved on the astral plane

### Information required

Here is a rough outline of the inputs that are needed in order to compute
a vote as part of the consensus algorithm.

- Configuration about how to contact this directory authority.
  - Used to compute our `dir-source` and `contact` lines.
  - This includes information on how to contact the authority _as a relay_.

- A configured set of consensus parameters.
  - Used to compute the `params` line.
  - The operator should be able to configure parameters here even if the
    software does not recognize them.

- A configured voting schedule, information about where we are within
  that schedule, and the current time.
  - Used to compute the `published`, `valid-after`, `fresh-until`,
    `valid-until`, and `voting-delay` lines.

- A configured list of recommended versions for client and server software.
  - Used to compute the `client-versions` and `server-versions` lines.
  - Optional. If absent, we don't include the lines based on it.

- A set of configured options to configure flag assignment:
  - In C tor these include:
    - AuthDirFastGuarantee
    - AuthDirGuardBWGuarantee
    - AuthDirListBadExits
    - AuthDirListMiddleOnly
    - AuthDirMaxServersPerAddr
    - AuthDirVoteGuardBwThresholdFaction
    - AuthDirVoteGuardGuaranteeTimeKnown
    - AuthDirVoteGuardGuaranteeWFU
    - AuthDirVoteStableGuaranteeMinUptime
    - AuthDirVoteStableGuaranteeMTBF
    - MinMeasuredBWsForAuthToIgnoreAdvertised
    - MinUptimeHidServDirectoryV2

- A set of options that control which relays are acceptable:
  - The minimal allowable relay software version.
  - The maximum number of relays per IPv4 address.
    - Used to calculate the Sybil flag.
  - For testing, a configuration flag to indicate whether relays with
    private addresses are acceptable.
  - (In C tor, these are used to reject descriptors as they are uploaded.)

- A configured list of relay identities, address patterns,
  and country codes that must receive special treatment.
  - This special treatment can include the following (listed with the
    equivalent C tor options):
    - Assign the BadExit flag. (AuthDirBadExit, AuthDirBadExitCCs)
    - Assign the MiddleOnly flag. (AuthDirMiddleOnly, AuthDirMiddleOnlyCCs)
    - Assign the Guard flag. (AuthDirGuard)
    - Do not assign the Valid flag. (AuthDirInvalid, AuthDirInvalidCCs)
    - Do not include the relay in any votes. (AuthDirReject, AuthDirRejectCCs)
      - In C tor this is also used to refuse votes as they arrive.

- A **hardwired** set of recommended or required subprotocol capabilities.
  - Used to produce `{recommended,required}-{client,relay}-protocols`.
  - This is hardwired to prevent Very Bad Outcomes.

- A bandwidth measurement file.
  - Produced asynchronously by a bandwidth scanner like SBWS.
  - Declares a measured bandwidth for each relay.
  - Used to produce the `w=` line in each routerstatus, and the Fast flag.
  - Used as an input for the Guard flag.
  - Served directly via http.
  - May be absent if we do not have an associated bandwidth authority.

- Our own authority certificate, and the authority signing key
  (`KS_auth_sign_rsa`) that it certifies.
  - This is pasted into the vote verbatim; the key is used to sign it.

- A set of router descriptors
  - Uploaded by relays.
  - Used to produce many microdescriptors, and many routerstatus elements.

- The current state of the shared-random-value protocol.
  - Persistent state.
  - Based on our own commitment, and commitments/reveals from other dirauths.
  - Used to calculate `shared-rand-*`

- For each relay, a set of _persistent_ measurements.
  - These measurements must be persistent across reboots.
  - In C tor, the authority (running as a relay) measures these things itself.
  - The measurements include:
     - Whether we have been able to contact it recently (45 minutes according to
       the spec), on all published OR ports.
       - Used to compute the Running flag.
     - A time-weighted Mean Time Between Failures (qv) for each relay.
       - Used to compute the Stable flag.
     - A Weighted Fractional Uptime (qv) for reach relay.
       - Used as an input for the Guard flag.
     - The time at which the relay first published a descriptor,
       and its weighted time known (qv).
       - Used as an input for the Guard flag.

- A persistent key-pinning record of which relays' legacy RSA identity keys
  (`KP_relayid_rsa`) are associated with which Ed25519 identity keys
  (`KP_relayid_ed`).
  - This is used to reject incoming descriptors that violate key pinning.
    Technically, we do that before the vote algorithm.

- Hopefully not a "guard fraction" file?
  - I think this is deprecated and unused. It used to track how much of the
    time for the last N months each relay had spent _as_ a guard.

- Later, if we add support for consensus transparency (and why not),
  a digest of our previous vote, and our previous consensus.
   - This will need a spec.

### Order of operations for computing a vote

(Very rough!)

- Remove rejected routers.
  - (We have to do this first so that they don't influence the flag assignments.)
- Compute thresholds for flag assignments
  - (This uses percentiles on our measured observations to set thresholds
    required to assign various flags, including Guard, Stable, Fast, etc)
  - (Only some routers are counted here, based on whether they are running,
    valid, etc.)
- For each router, compute the routerstatus we want to include for it in our vote.
  This includes generating microdescriptors and assigning flags.)

## deployment transition plan

Directory consensus protocol means that
if we change the consensus algorithm
at least 1/3 of functioning dirauths, and probably more,
must change simultaneously.
(We go from \<1/3 new to \>2/3 new in one go.)

We think it is probably not going to be feasible to precisely reproduce
the consensus calculations from C Tor in Arti.

This is practical only if the simultaneously-switching dirauths
all implement both the old and new consensus algorithm.
(This is what the consensus methods are for.)
We can't switch all dirauths from C Tor to Arti on the same day.

Instead, we will
make the Arti implementation of the consensus protocol
available in a form that can be used by C Tor.
We'll
adapt C Tor to be able to call that implementation,
making the choice based on the consensus method.

When enough (C Tor) dirauths have the Arti consensus algorithm available,
the consensus method protocol will automatically switch
to using the Arti consensus.

After that, C Tor dirauths without the Arti consensus algorithm
will effectively not participate, until they are upgraded.
But pure-Arti dirauths (which can only perform the Arti consensus algorithm)
can be deployed.

(In practice there may be, during the transition,
more than one relevant Arti consensus method
and possibly more than one relevant C Tor consensus method.)

### Rationale, dirauth upgrade impact

Arti dirauth is not going to be a drop-in replacement
for C Tor dirauth.
While we'll aim to minimise unnecessary changes,
it will interact with the operating system somewhat differently,
be configured somewhat differently,
and there will be possible complications involving key management.

So the upgrade process for each dirauth
will involve human work by the operator,
and carries some risk.
It is likely to involve some downtime.

Attempting to do this near-simultaneously for all dirauths
has a big coordination problem and risks a long outage.

Ideally dirauth upgrades would be staggered,
to maximise availability and minimise risk.

### dirauth operator options

Each dirauth operator can choose
from the following options,
(presented in order from least to most effort):

 1. Do nothing until the network consensus
    is using the Arti consensus method,
    at which point their dirauth ceases to be part of the consensus.
    Then, upgrade straight to Arti dirauth at operator's convenience.

    The transition plan depends on no more than
    1/3 of dirauth operators choosing this option -
    ideally, fewer.

 2. Install the Arti dirauth plugin when it becomes available,
    and tell C Tor to load/use it.
    Eventually, when Arti consensuses are stable, upgrade to Arti dirauth.
    This dirauth will participate in the consensus
    throughout the transition.
    Low-latency communication with and quick response by the operator
    is not required.

 3. Install the Arti dirauth plugin,
    but initially configure it to run only in a testing mode -
    ie, don't advertise the Arti consensus method.
    Engage with the transition scheduling team
    (Arti team, Network Health team, interested dirauths)
    and be part of the coordinated configuration change
    to switch to the Arti consensus method.
    Eventually, when Arti consensuses are stable, upgrade to Arti dirauth.
    We need at least a handful of these,
    depending precisely on what options everyone picks.

 4. Switch over to Arti dirauth as soon as possible.
    These dirauths will not participate in consensuses
    until the consensus switches to the Arti method.

    These operators can provide valuable feedback on Arti dirauth,
    but having many dirauths in this state reduces network resilience,
    so ideally this would be a minority choice.
    Ideally we would have at least one dirauth operator in this category,
    so we can discover issues with Arti dirauth as soon as possible,
    but that's not essential for the transition plan.

dirauth operators may change their mind,
moving from one category to another,
but for simplicity we'll write as if
each dirauth is in a fixed category determined at the start.

### Detailed schedule

 * Phase 1: software development.

   Discussions with dirauth operators, Network Health team,
   about requirements, planning, etc.

   Arti team develops:
     - Arti dirauth
     - Arti consensus method plugin for C Tor
     - C Tor configuration for using Arti consensus method plugin

   dirauth operators provide feedback, additional testing, etc.

   There are likely to be updates to C Tor to tidy up
   some aspects of the Tor protocols which we don't want to reimplement.
   These will be released and deployed according to normal C Tor processes.

 * Milestone 1: Software available.

   The Arti project is shipping both
    1. Arti dirauth
    2. the Arti consensus method plugin and its support in C Tor
   as formal software deliverables,
   in a form suitable for production use by dirauth operators.

   Any necessary updates to C Tor dirauths (and maybe relays)
   for compatibility with Arti votes and consensuses
   have been deployed.

   Schedule determined by: software development timescale.

 * Phase 2: deployment of support for the Arti consensus method.

   dirauths in category 4 switch to Arti dirauth
   (and stop running C Tor entirely).
   Each of these dirauths will be down during its transition.

   dirauths in categories 2 and 3 install the Arti dirauth plugin,
   and configure their C Tor accordingly.
 
 * Milestone 2: Arti consensus method available.

   At least 2/3 of dirauths have the Arti consensus method available
   (ie, are in categories 2-4 and have completed their phase 2 setup).

   Schedule determined by: dirauth operators' deployment decisions.

 * Phase 3: switch to the Arti consensus method.

   dirauths in category 3 coordinate,
   and switch their configuration to advertise the Arti consensus method.

   The Tor network consensus switches over.
   Category 4 dirauths now participate in consensus;
   category 1 dirauths no longer participate in consensus.
   We monitor the network behaviour,
   ready to revert if we see problems.

   Schedule determined by:
   explicit decision by category 3 dirauth operators
   as advised by Arti experts, Network Health team, etc.

 * Milestone 3: we believe the Arti consensus method is stable.

   Schedule determined by:
   explicit decision by category 3 dirauth operators
   as advised by Arti experts, Network Health team, etc.

 * Phase 4: deployment of Arti dirauth

   dirauths (in categories 1-3) install Arti dirauth and deinstall C Tor,
   on their own schedule.
   Each of these dirauths will be down during its transition;
   some coordination is advisable to reduce overall network impact.

 * Milestone 4: C Tor dirauth withdrawn.

   All (or nearly all) dirauths are running Arti dirauth
   (not C Tor with Arti plugin).
   C Tor dirauth can be desupported.
