// What is recorded, as the pages offer it: one choice of three, kept by the radio as two settings
// (`recording.enabled` and `recording.every_call`).

// [key, on the button, what it means, what is sent].
export const RECORD = [
  ['off', 'Off', 'No call is recorded', { enabled: false }],
  ['marked', 'Marked', 'Only the calls of the talkgroups marked Rec are recorded', { enabled: true, every_call: false }],
  ['every', 'Every call', 'Every followed call is recorded', { enabled: true, every_call: true }],
];

// The choice the radio holds.
export const recordKey = r => (!r || !r.enabled ? 'off' : r.every_call ? 'every' : 'marked');
