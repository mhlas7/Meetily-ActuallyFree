import { expect, mock, test } from 'bun:test';
const calls: Array<[string, unknown]> = [];
mock.module('@tauri-apps/api/core', () => ({ invoke: async (name: string, args: unknown) => { calls.push([name,args]); } }));
mock.module('@tauri-apps/api/event', () => ({ listen: async () => () => {} }));
const { RecordingService } = await import('../../src/services/recordingService');

test('device selections and meeting name use Tauri camelCase IPC keys', async () => {
  await new RecordingService().startRecordingWithDevices('USB (input)', 'Headphones (output)', 'Chosen title');
  expect(calls.at(-1)).toEqual(['start_recording_with_devices_and_meeting', {
    micDeviceName:'USB (input)', systemDeviceName:'Headphones (output)', meetingName:'Chosen title',
  }]);
});
