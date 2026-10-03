# Test fixtures

| File | Content |
| --- | --- |
| `sine.taf` | 5 s stereo tone, 440 Hz left and 660 Hz right |
| `chapters.taf` | three chapters of 2 s each, same tones |

- Both are synthesised sine tones, written by an independent TAF writer.
- They contain no third-party audio, no stock content and no Tonie data.
- The tests treat them as frozen data; nothing in the repository regenerates them.
