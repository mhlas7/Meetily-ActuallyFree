import { describe, expect, test } from 'bun:test';
import {
  deviceDisplayName,
  deviceSelectValue,
  preferenceForSelection,
  resolveSelectedDeviceName,
  toDeviceOptionValue,
} from '../../src/lib/audio-devices';

describe('toDeviceOptionValue', () => {
  test('matches the string persisted in preferences', () => {
    expect(toDeviceOptionValue({ name: 'USB Audio Headphones (System Audio)', device_type: 'Output' }))
      .toBe('USB Audio Headphones (System Audio) (output)');
  });

  test('lowercases the device type', () => {
    expect(toDeviceOptionValue({ name: 'Built-in Mic', device_type: 'Input' }))
      .toBe('Built-in Mic (input)');
  });
});

describe('deviceSelectValue', () => {
  const available = ['Built-in Mic (input)', 'JBL Tune 770NC (input)'];

  test('keeps a saved device that is still available', () => {
    expect(deviceSelectValue('JBL Tune 770NC (input)', available)).toBe('JBL Tune 770NC (input)');
  });

  test('falls back to the default sentinel when the saved device is gone', () => {
    // The disconnected-Bluetooth case: previously left the picker blank.
    expect(deviceSelectValue('Disconnected Headset (input)', available)).toBe('default');
  });

  test('returns the default sentinel when nothing is saved', () => {
    expect(deviceSelectValue(null, available)).toBe('default');
  });

  test('falls back while the device list is still empty', () => {
    expect(deviceSelectValue('JBL Tune 770NC (input)', [])).toBe('default');
  });

  test('does not match on a partial name', () => {
    expect(deviceSelectValue('JBL Tune 770NC', available)).toBe('default');
  });
});

describe('deviceDisplayName', () => {
  test('strips the output suffix', () => {
    expect(deviceDisplayName('JBL Tune 770NC (System Audio) (output)'))
      .toBe('JBL Tune 770NC (System Audio)');
  });

  test('strips the input suffix', () => {
    expect(deviceDisplayName('Built-in Mic (input)')).toBe('Built-in Mic');
  });

  test('leaves a name with no suffix alone', () => {
    expect(deviceDisplayName('Built-in Mic')).toBe('Built-in Mic');
  });

  test('only strips a trailing suffix, not one mid-name', () => {
    expect(deviceDisplayName('Mic (input) Dock (output)')).toBe('Mic (input) Dock');
  });
});

describe('resolveSelectedDeviceName', () => {
  test('resolves the default sentinel to the reported system default', () => {
    expect(resolveSelectedDeviceName('default', 'USB Audio Headphones')).toBe('USB Audio Headphones');
  });

  test('passes an explicitly chosen device through unchanged', () => {
    expect(resolveSelectedDeviceName('Built-in Mic', 'USB Audio Headphones')).toBe('Built-in Mic');
  });

  test('yields an empty name when no system default is available', () => {
    expect(resolveSelectedDeviceName('default', null)).toBe('');
  });

  test('treats an empty selection as the default', () => {
    expect(resolveSelectedDeviceName('', 'Built-in Mic')).toBe('Built-in Mic');
  });
});

describe('preferenceForSelection', () => {
  test('stores null for the default sentinel so the backend follows the system default', () => {
    expect(preferenceForSelection('default', 'Input')).toBeNull();
  });

  test('stores null for an empty selection', () => {
    expect(preferenceForSelection('', 'Output')).toBeNull();
  });

  test('stores an explicit microphone in the same format the settings picker uses', () => {
    expect(preferenceForSelection('Built-in Mic', 'Input')).toBe('Built-in Mic (input)');
  });

  test('stores an explicit system device in the same format the settings picker uses', () => {
    expect(preferenceForSelection('USB Audio Headphones (System Audio)', 'Output'))
      .toBe('USB Audio Headphones (System Audio) (output)');
  });
});
