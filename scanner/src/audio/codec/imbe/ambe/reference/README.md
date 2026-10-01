# jmbe reference for the AMBE+2 port

`AmbeReference.java` decodes 9-byte AMBE frames with the jmbe 1.0.9 jar (the version this port follows) and writes the PCM the Rust tests compare against (`../tests.rs`). It seeds jmbe's comfort-noise `Random` (seed 20260930) so erasure and muted frames match too.

Run from `p25-httpd/src/jmbe/ambe` in Git Bash (JDK 11+; the jars are SDRTrunk's jmbe folder):

```sh
J=C:/Users/Andy/SDRTrunk/jmbe
CP="$J/jmbe-1.0.9.jar;$J/jmbe-api.jar;$J/JTransforms-3.1.jar;$J/JLargeArrays-1.6.jar;$J/commons-math3-3.6.1.jar;$J/slf4j-api-2.0.17.jar"
javac -cp "$CP" -d /tmp/ambe-ref reference/AmbeReference.java
java -cp "/tmp/ambe-ref;$CP" AmbeReference test_frames_clay_ts2.bin test_pcm_jmbe_clay_ts2.f32 [out.wav] [frames.txt]
java -cp "/tmp/ambe-ref;$CP" AmbeReference test_frames_synthetic.bin test_pcm_jmbe_synthetic.f32
```

The frame fixtures come from the `#[ignore]` tests `build_frame_fixture` (needs the capture) and `build_synthetic_fixture`. `frames.txt` lists jmbe's FEC, b-vector, metadata and model parameters per frame; `../frame_tests.rs` holds its FEC columns.

`gen_tables.py` regenerates `../tables.rs` from the jmbe v1.0.9 sources: `python reference/gen_tables.py <jmbe>/codec/src/main/java/jmbe/codec/ambe tables.rs`.
