-- Where each speaker-to-contact link came from, so automatic voice updates can
-- learn only from names the user set or from confident voice matches.
-- 'user': a manual relabel. 'match': a post-call voice-profile match, with its
-- cosine score and lead over the next voice. 'saved': linked when the meeting
-- was saved, where a typed live name and a live voice match look the same.
ALTER TABLE person_speakers ADD COLUMN source TEXT NOT NULL DEFAULT 'saved';
ALTER TABLE person_speakers ADD COLUMN match_score REAL;
ALTER TABLE person_speakers ADD COLUMN match_margin REAL;
