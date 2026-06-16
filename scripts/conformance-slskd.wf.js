export const meta = {
  name: 'conformance-slskd',
  description: 'Audit soulseek-proto + core subsystems against Soulseek.NET (slskd dep); resolve divergences by adjudicating slskd vs nicotine-plus case-by-case; draft tests',
  phases: [
    { title: 'Compare', detail: 'one agent per message: our codec vs Soulseek.NET C#' },
    { title: 'Resolve', detail: 'divergent messages: adjudicate slskd vs nicotine-plus on the merits, draft test' },
    { title: 'Subsystems', detail: 'shares / search / transfers behavioral parity' },
  ],
}

// Repo roots
const PROTO = 'crates/soulseek-proto/src'
const SSNET = '/home/berend/Developer/Soulseek.NET/src/Messaging/Messages'
const NICO = '/home/berend/Developer/nicotine-plus/pynicotine/slskmessages.py'

// Work-list: every message our proto implements, joined to its Soulseek.NET counterpart by wire identity.
// ssnet is a hint filename under SSNET/<group>/ ; the agent confirms/locates the real file.
const MESSAGES = [
  // --- Server (PROTO/server.rs) ---
  { name: 'LoginRequest', code: 'Server 1', our: 'server.rs:LoginRequest', group: 'Server', ssnet: 'LoginRequest.cs' },
  { name: 'LoginResponse', code: 'Server 1', our: 'server.rs:LoginResponse', group: 'Server', ssnet: 'LoginResponse.cs' },
  { name: 'SetWaitPort', code: 'Server 2', our: 'server.rs:SetWaitPort', group: 'Server', ssnet: 'SetListenPortCommand.cs' },
  { name: 'GetPeerAddressRequest', code: 'Server 3', our: 'server.rs:GetPeerAddressRequest', group: 'Server', ssnet: 'GetPeerAddress*.cs' },
  { name: 'GetPeerAddressResponse', code: 'Server 3', our: 'server.rs:GetPeerAddressResponse', group: 'Server', ssnet: 'GetPeerAddress*.cs' },
  { name: 'FileSearchRequest', code: 'Server 26', our: 'server.rs:FileSearchRequest', group: 'Server', ssnet: 'SearchRequest.cs' },
  { name: 'FileSearchBroadcast', code: 'Server 26', our: 'server.rs:FileSearchBroadcast', group: 'Server', ssnet: 'ServerSearchRequest.cs' },
  { name: 'ConnectToPeerRequest', code: 'Server 18', our: 'server.rs:ConnectToPeerRequest', group: 'Server', ssnet: 'ConnectToPeerRequest.cs' },
  { name: 'ConnectToPeer', code: 'Server 18', our: 'server.rs:ConnectToPeer', group: 'Server', ssnet: 'ConnectToPeerResponse.cs' },
  { name: 'ExcludedSearchPhrases', code: 'Server 160', our: 'server.rs:ExcludedSearchPhrases', group: 'Server', ssnet: 'ExcludedSearchPhrasesNotification.cs' },
  { name: 'BranchLevel', code: 'Server 126', our: 'server.rs:BranchLevel', group: 'Server', ssnet: 'BranchLevelCommand.cs' },
  { name: 'BranchRoot', code: 'Server 127', our: 'server.rs:BranchRoot', group: 'Server', ssnet: 'BranchRootCommand.cs' },
  { name: 'ParentMinSpeed', code: 'Server 83', our: 'server.rs:ParentMinSpeed', group: 'Server', ssnet: 'IntegerResponse.cs' },
  { name: 'ParentSpeedRatio', code: 'Server 84', our: 'server.rs:ParentSpeedRatio', group: 'Server', ssnet: 'IntegerResponse.cs' },
  { name: 'PossibleParents/NetInfo', code: 'Server 102', our: 'server.rs:PossibleParents', group: 'Server', ssnet: 'NetInfoNotification.cs' },
  { name: 'EmbeddedMessage', code: 'Server 93', our: 'server.rs:EmbeddedMessage', group: 'Server', ssnet: 'EmbeddedMessage*.cs' },

  // --- Peer (PROTO/peer_message.rs) ---
  { name: 'GetSharedFileList/BrowseRequest', code: 'Peer 4', our: 'peer_message.rs:GetSharedFileList', group: 'Peer', ssnet: 'BrowseRequest.cs' },
  { name: 'SharedFileListResponse/BrowseResponse', code: 'Peer 5', our: 'peer_message.rs:SharedFileListResponse', group: 'Peer', ssnet: 'BrowseResponseFactory.cs' },
  { name: 'FileSearchResponse/SearchResponse', code: 'Peer 9', our: 'peer_message.rs:FileSearchResponse', group: 'Peer', ssnet: 'SearchResponseFactory.cs' },
  { name: 'FolderContentsRequest', code: 'Peer 36', our: 'peer_message.rs:FolderContentsRequest', group: 'Peer', ssnet: 'FolderContentsRequest.cs' },
  { name: 'FolderContentsResponse', code: 'Peer 37', our: 'peer_message.rs:FolderContentsResponse', group: 'Peer', ssnet: 'FolderContentsResponse.cs' },
  { name: 'UserInfoRequest', code: 'Peer 15', our: 'peer_message.rs:UserInfoRequest', group: 'Peer', ssnet: 'UserInfoRequest.cs' },
  { name: 'UserInfoResponse', code: 'Peer 16', our: 'peer_message.rs:UserInfoResponse', group: 'Peer', ssnet: 'UserInfoResponseFactory.cs' },

  // --- Distributed (PROTO/distributed.rs) ---
  { name: 'DistribPing', code: 'Distributed 0', our: 'distributed.rs:DistribPing', group: 'Distributed', ssnet: 'DistributedPingRequest.cs' },
  { name: 'DistribSearch', code: 'Distributed 3', our: 'distributed.rs:DistribSearch', group: 'Distributed', ssnet: 'DistributedSearchRequest.cs' },
  { name: 'DistribBranchLevel', code: 'Distributed 4', our: 'distributed.rs:DistribBranchLevel', group: 'Distributed', ssnet: 'DistributedBranchLevel.cs' },
  { name: 'DistribBranchRoot', code: 'Distributed 5', our: 'distributed.rs:DistribBranchRoot', group: 'Distributed', ssnet: 'DistributedBranchRoot.cs' },
  { name: 'DistribChildDepth', code: 'Distributed 7', our: 'distributed.rs:DistribChildDepth', group: 'Distributed', ssnet: 'DistributedChildDepth.cs' },
  { name: 'DistribEmbedded', code: 'Distributed 93', our: 'distributed.rs:DistribEmbedded', group: 'Distributed', ssnet: 'EmbeddedMessage*.cs' },

  // --- Peer init (PROTO/peer.rs) ---
  { name: 'PierceFirewall', code: 'Init 0', our: 'peer.rs:PierceFirewall', group: 'Initialization', ssnet: 'PierceFirewall.cs' },
  { name: 'PeerInit', code: 'Init 1', our: 'peer.rs:PeerInit', group: 'Initialization', ssnet: 'PeerInit.cs' },

  // --- Transfer (PROTO/transfer.rs) ---
  { name: 'TransferRequest', code: 'Peer 40', our: 'transfer.rs:TransferRequest', group: 'Peer', ssnet: 'TransferRequest.cs' },
  { name: 'TransferResponse', code: 'Peer 41', our: 'transfer.rs:TransferResponse', group: 'Peer', ssnet: 'TransferResponse.cs' },
  { name: 'QueueUpload/QueueDownload', code: 'Peer 43', our: 'transfer.rs:QueueUpload', group: 'Peer', ssnet: 'QueueDownloadRequest.cs' },
  { name: 'PlaceInQueueRequest', code: 'Peer 51', our: 'transfer.rs:PlaceInQueueRequest', group: 'Peer', ssnet: 'PlaceInQueueRequest.cs' },
  { name: 'PlaceInQueueResponse', code: 'Peer 44', our: 'transfer.rs:PlaceInQueueResponse', group: 'Peer', ssnet: 'PlaceInQueueResponse.cs' },
  { name: 'UploadDenied', code: 'Peer 50', our: 'transfer.rs:UploadDenied', group: 'Peer', ssnet: 'UploadDenied.cs' },
  { name: 'UploadFailed', code: 'Peer 46', our: 'transfer.rs:UploadFailed', group: 'Peer', ssnet: 'UploadFailed.cs' },
  { name: 'FileTransferInit', code: 'transfer-conn init', our: 'transfer.rs:FileTransferInit', group: 'transfer-conn', ssnet: '(transfer connection handshake: 4-byte token)' },
  { name: 'FileOffset', code: 'transfer-conn offset', our: 'transfer.rs:FileOffset', group: 'transfer-conn', ssnet: '(transfer connection: 8-byte file offset)' },
]

const COMPARE_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  required: ['message', 'our_location', 'ssnet_file', 'conformant', 'confidence', 'divergences', 'notes'],
  properties: {
    message: { type: 'string' },
    our_location: { type: 'string', description: 'file:line of our impl' },
    ssnet_file: { type: 'string', description: 'the Soulseek.NET file actually used as reference, or "NOT FOUND"' },
    conformant: { type: 'boolean' },
    confidence: { type: 'string', enum: ['high', 'medium', 'low'] },
    divergences: {
      type: 'array',
      items: {
        type: 'object',
        additionalProperties: false,
        required: ['field', 'ours', 'soulseek_net', 'severity'],
        properties: {
          field: { type: 'string' },
          ours: { type: 'string' },
          soulseek_net: { type: 'string' },
          severity: { type: 'string', enum: ['breaking', 'minor', 'cosmetic'] },
        },
      },
    },
    notes: { type: 'string' },
  },
}

const RESOLVE_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  required: ['message', 'nicotine_location', 'nicotine_behavior', 'ssnet_behavior', 'nicotine_agrees_with_ssnet', 'follow', 'decision_rationale', 'verdict', 'proposed_test_name', 'proposed_test_target_file', 'proposed_test_code'],
  properties: {
    message: { type: 'string' },
    nicotine_location: { type: 'string', description: 'slskmessages.py:line or "NOT FOUND"' },
    nicotine_behavior: { type: 'string' },
    ssnet_behavior: { type: 'string' },
    nicotine_agrees_with_ssnet: { type: 'boolean' },
    follow: { type: 'string', enum: ['soulseek_net', 'nicotine', 'both_agree', 'already_correct', 'false_positive'] },
    decision_rationale: { type: 'string', description: 'when the two references disagree, WHY the chosen one is correct (evidence: protocol docs, real-network/soulfind behavior, deprecation notes, code comments, internal consistency). "n/a" if they agree or false positive.' },
    verdict: { type: 'string', description: 'the correct wire behavior we must conform to' },
    proposed_test_name: { type: 'string', description: 'snake_case rust test fn name' },
    proposed_test_target_file: { type: 'string' },
    proposed_test_code: { type: 'string', description: 'a complete #[test] fn (encode and/or decode) asserting the correct behavior; self-contained, uses crate APIs' },
  },
}

const SUBSYS_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  required: ['subsystem', 'ssnet_refs', 'our_refs', 'findings'],
  properties: {
    subsystem: { type: 'string' },
    ssnet_refs: { type: 'array', items: { type: 'string' } },
    our_refs: { type: 'array', items: { type: 'string' } },
    findings: {
      type: 'array',
      items: {
        type: 'object',
        additionalProperties: false,
        required: ['title', 'ours', 'soulseek_net', 'severity', 'recommendation'],
        properties: {
          title: { type: 'string' },
          ours: { type: 'string' },
          soulseek_net: { type: 'string' },
          severity: { type: 'string', enum: ['breaking', 'minor', 'cosmetic'] },
          recommendation: { type: 'string' },
        },
      },
    },
  },
}

const comparePrompt = (m) => `You are auditing wire-protocol conformance of the Rust Soulseek client "soulrust" against the reference C# library Soulseek.NET (the protocol slskd v10.0.1 depends on).

MESSAGE: ${m.name}  (wire code: ${m.code})
OUR IMPL: ${PROTO}/${m.our.split(':')[0]}  — symbol "${m.our.split(':')[1]}"
SOULSEEK.NET REFERENCE: under ${SSNET}/${m.group}/ ; hint file: ${m.ssnet}

Steps:
1. Read our impl: ${PROTO}/${m.our.split(':')[0]} (find symbol "${m.our.split(':')[1]}" and its encode/decode). Also read ${PROTO}/wire.rs to understand our primitive encoders (string=u32-len+bytes, ints LE, etc.).
2. Locate and read the Soulseek.NET reference file (grep under ${SSNET}/${m.group}/ for the message; check ToByteArray()/the Reader-based constructor for the exact field order, types, and lengths). If the hint file is wrong, find the right one. Some are "Factory" classes.
3. Compare BYTE LAYOUT precisely: field order, integer widths/endianness, string length-prefix width, booleans (byte vs int), optional/trailing fields, compression (zlib for browse/folder responses), and the message code itself.

Report via the StructuredOutput tool. conformant=false ONLY for a real wire-layout divergence (something that would mis-encode/decode against a real peer). Naming differences are NOT divergences. If you cannot find the Soulseek.NET reference, set ssnet_file="NOT FOUND", conformant=true, confidence="low", and explain in notes. Be precise; cite line numbers in ours/soulseek_net fields.`

const resolvePrompt = (cmp) => `A wire-conformance comparison flagged the Soulseek message "${cmp.message}" as possibly NON-conformant in the Rust client soulrust vs Soulseek.NET.

Divergences found:
${cmp.divergences.map(d => `- [${d.severity}] ${d.field}: ours=${d.ours} | soulseek.net=${d.soulseek_net}`).join('\n')}
Our impl: ${cmp.our_location}. Soulseek.NET ref: ${cmp.ssnet_file}.

Your job — resolve the ground truth and draft a test:
1. Open nicotine-plus's protocol definitions at ${NICO} and find this message (search by class name / message code ${cmp.message}). Read how it packs/unpacks the relevant field(s). nicotine-plus also ships protocol documentation (look for doc/SLSKPROTOCOL or docstrings in slskmessages.py) — consult it.
2. Re-read the Soulseek.NET reference (${SSNET}) and, if needed, our Rust impl (${PROTO}) to confirm the real divergence.
3. Decide the CORRECT wire behavior:
   - If nicotine-plus and Soulseek.NET AGREE → that's the truth (set follow="both_agree"); our code is likely the bug.
   - If they DISAGREE → DO NOT blindly pick one. Adjudicate on the merits, case by case. Weigh the available evidence and decide which is actually correct: which matches the documented Soulseek protocol; which matches what the real network / a real server (soulfind) would accept; recency and maintenance (is one side's handling marked deprecated or carrying a "the server actually sends X" comment); internal consistency with neighboring messages; and which interpretation a real peer would tolerate. Pick the one the evidence supports — it may be nicotine-plus OR Soulseek.NET. Set follow accordingly and EXPLAIN your reasoning in decision_rationale with the concrete evidence. If the evidence is genuinely ambiguous, prefer the interpretation most likely to interoperate with the live network and say so.
   - If on closer reading our code is actually correct, set follow="false_positive".
4. Draft a complete Rust #[test] fn that pins the correct (adjudicated) behavior: build the message, encode it (and/or decode a known byte vector), and assert the bytes/fields match the verdict. It must compile against the soulseek-proto crate APIs you saw in ${PROTO} (look at existing tests in crates/soulseek-proto/tests/nicotine_vectors.rs for the encode/decode helpers and style). Put it in proposed_test_target_file = crates/soulseek-proto/tests/slskd_conformance.rs unless a better location exists.

Report via StructuredOutput.`

// ---- Phase 1+2: compare each message, resolve divergences inline (pipeline, no barrier) ----
phase('Compare')
const perMessage = await pipeline(
  MESSAGES,
  (m) => agent(comparePrompt(m), { label: `cmp:${m.name}`, phase: 'Compare', schema: COMPARE_SCHEMA, effort: 'high' }),
  (cmp, m) => {
    if (!cmp) return null
    if (cmp.conformant || !cmp.divergences || cmp.divergences.length === 0) {
      return { compare: cmp, resolve: null }
    }
    return agent(resolvePrompt({ ...cmp, our_location: cmp.our_location || `${PROTO}/${m.our}` }),
      { label: `resolve:${cmp.message}`, phase: 'Resolve', schema: RESOLVE_SCHEMA, effort: 'high' })
      .then(res => ({ compare: cmp, resolve: res }))
  }
)

// ---- Phase 3: subsystem behavioral parity ----
phase('Subsystems')
const SUBSYS = [
  {
    subsystem: 'shares',
    prompt: `Compare the SHARES subsystem of soulrust (Rust, crates/soulrust/src/shares/ and the browse/folder response building in crates/soulseek-proto/src/peer_message.rs) against Soulseek.NET (/home/berend/Developer/Soulseek.NET/src/Shares/ and Messaging/Messages/Peer/BrowseResponseFactory.cs + FolderContentsResponse.cs). Focus on protocol-facing behavior: how the shared file list is structured (directories, files, attributes: bitrate/duration/sample-rate/sample-size codes), zlib compression of browse responses, path separators (backslash), and unshared/locked-dir handling. List concrete divergences with file:line on both sides and a recommendation. Where wire-facing behavior differs and matters, recommendation should note a test to add.`,
  },
  {
    subsystem: 'search',
    prompt: `Compare the SEARCH subsystem of soulrust (Rust: crates/soulrust/src/search_response.rs, crates/soulrust/src/components/ search-related files, and FileSearchResponse in crates/soulseek-proto/src/peer_message.rs) against Soulseek.NET (/home/berend/Developer/Soulseek.NET/src/Search/ and Messaging/Messages/Peer/SearchResponseFactory.cs + Server/SearchRequest.cs). Focus on: search ticket/token handling, query sanitization/tokenization sent on the wire, search-result record layout (filename, size, ext, attributes, free-upload-slot, avg speed, queue length, locked results), and result filtering. List concrete divergences with file:line and recommendations.`,
  },
  {
    subsystem: 'transfers',
    prompt: `Compare the TRANSFERS subsystem of soulrust (Rust: crates/soulrust/src/transfers/ and crates/soulseek-proto/src/transfer.rs) against Soulseek.NET (/home/berend/Developer/Soulseek.NET/src/Transfers/ and Messaging/Messages/Peer/Transfer*.cs, QueueDownloadRequest.cs, PlaceInQueue*.cs, UploadDenied/Failed). Focus on the transfer handshake state machine: TransferRequest/Response direction & ticket semantics, queueing (QueueUpload), the 'F' transfer-connection init token, the 8-byte file offset exchange, allowed/denied reasons strings, and place-in-queue flow. List concrete divergences with file:line and recommendations.`,
  },
]
const subsystems = await parallel(SUBSYS.map(s => () =>
  agent(s.prompt, { label: `subsys:${s.subsystem}`, phase: 'Subsystems', schema: SUBSYS_SCHEMA, effort: 'high' })))

// ---- Assemble ----
const rows = perMessage.filter(Boolean)
const compared = rows.map(r => r.compare)
const nonConformant = rows.filter(r => r.resolve).map(r => r.resolve)
const realDivergences = nonConformant.filter(r => r.follow !== 'false_positive' && r.follow !== 'already_correct')
const proposedTests = realDivergences.map(r => ({
  name: r.proposed_test_name,
  target: r.proposed_test_target_file,
  message: r.message,
  follow: r.follow,
  verdict: r.verdict,
  code: r.proposed_test_code,
}))

return {
  summary: {
    messages_compared: compared.length,
    conformant: compared.filter(c => c.conformant).length,
    flagged: nonConformant.length,
    confirmed_divergences: realDivergences.length,
    false_positives: nonConformant.filter(r => r.follow === 'false_positive' || r.follow === 'already_correct').length,
    not_found: compared.filter(c => c.ssnet_file === 'NOT FOUND').length,
    subsystem_findings: subsystems.filter(Boolean).reduce((n, s) => n + (s.findings?.length || 0), 0),
  },
  divergences: realDivergences,
  proposed_tests: proposedTests,
  all_comparisons: compared,
  subsystems: subsystems.filter(Boolean),
}
