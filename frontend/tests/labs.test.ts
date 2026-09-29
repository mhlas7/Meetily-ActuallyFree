import { describe, expect, test } from 'bun:test';
import { cleanTranscriptText } from '../src/lib/labs';

describe('Labs clean transcript display', () => {
  test('removes a hesitation and immediate stutter without changing the input', () => {
    const raw = 'Um, we we need to leave.';
    expect(cleanTranscriptText(raw)).toBe('We need to leave.');
    expect(raw).toBe('Um, we we need to leave.');
  });

  test('retains emphatic repetitions and substantive phrases', () => {
    expect(cleanTranscriptText('No no, that is very very important.'))
      .toBe('No no, that is very very important.');
    expect(cleanTranscriptText('You know the answer.')).toBe('You know the answer.');
  });
});
