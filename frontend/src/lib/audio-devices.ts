/**
 * Pure helpers for reconciling a saved audio-device preference against the
 * devices currently reported by the backend.
 *
 * Kept out of the component so they can be unit tested without React or Tauri.
 */

export interface AudioDeviceOption {
  name: string;
  device_type: 'Input' | 'Output';
}

/** The option value a device renders as, and the string persisted in preferences. */
export const toDeviceOptionValue = (d: AudioDeviceOption) =>
  `${d.name} (${d.device_type.toLowerCase()})`;

/**
 * Value a device <Select> should display for a saved preference.
 *
 * A saved string matching no <SelectItem> makes Radix render a BLANK trigger:
 * the placeholder only shows for '' or undefined, and a stale preference is
 * neither. Falling back to the 'default' sentinel keeps the picker readable and
 * mirrors what the backend already does at record time, where an unresolvable
 * device falls back to the system default.
 *
 * Display-only on purpose — it never writes, so the saved preference survives a
 * device being temporarily absent and returns when the device does.
 */
export function deviceSelectValue(saved: string | null, available: string[]): string {
  if (!saved) return 'default';
  return available.includes(saved) ? saved : 'default';
}

/** "JBL Tune 770NC (System Audio) (output)" -> "JBL Tune 770NC (System Audio)" */
export function deviceDisplayName(stored: string): string {
  return stored.replace(/\s*\((input|output)\)$/i, '');
}

/** Sentinel value for the "use whatever the system default is" picker entry. */
export const DEFAULT_DEVICE_OPTION = 'default';

/**
 * The device name to actually capture from for a given picker selection.
 *
 * `DEFAULT_DEVICE_OPTION` resolves to whatever the backend reports as the
 * current system default; an empty string means no default was available, which
 * callers treat as "nothing to meter".
 */
export function resolveSelectedDeviceName(
  selection: string,
  defaultName: string | null,
): string {
  if (!selection || selection === DEFAULT_DEVICE_OPTION) return defaultName ?? '';
  return selection;
}

/**
 * The value to persist in preferences for a picker selection.
 *
 * `null` means "follow the system default" — that is how the backend already
 * represents an unset preference, so choosing Default must clear the field
 * rather than pin the default device's current name.
 */
export function preferenceForSelection(
  selection: string,
  deviceType: 'Input' | 'Output',
): string | null {
  if (!selection || selection === DEFAULT_DEVICE_OPTION) return null;
  return toDeviceOptionValue({ name: selection, device_type: deviceType });
}
