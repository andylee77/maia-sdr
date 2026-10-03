import io.github.dsheirer.dsp.filter.FilterFactory;
import io.github.dsheirer.module.decode.p25.phase1.P25P1DecoderLSM;
import io.github.dsheirer.sample.complex.ComplexSamples;
import java.io.BufferedInputStream;
import java.io.ByteArrayOutputStream;
import java.io.DataInputStream;
import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.PrintStream;
import java.lang.reflect.Method;
import java.nio.ByteBuffer;

/**
 * Runs SDRTrunk's P25P1DecoderLSM headless, the reference for the scanner's LSM port (change 079).
 *
 * taps: prints the decoder's filters at 25 kSPS, `name|count|v0,v1,...` (the baseband low-pass
 * comes from the decoder's own private builder, so it is exactly what SDRTrunk runs).
 *
 * decode OUT_DIR WAV...: decodes 50 kSPS stereo int16 WAVs (SDRTrunk `_baseband.wav` recordings,
 * the scanner's /api/v1/iq/control.wav) from a fresh decoder each. Writes the demodulated dibits
 * to OUT_DIR/<name>.bits (four dibits per byte, first in the high bits, as SDRTrunk's `.bits`)
 * and prints one line per message: file|timestamp_ms|valid|class|toString.
 */
public class LsmWavHarness
{
    public static void main(String[] args) throws Exception
    {
        PrintStream out = new PrintStream(System.out, true, "UTF-8");
        if(args.length >= 1 && args[0].equals("taps"))
        {
            Method baseband = P25P1DecoderLSM.class.getDeclaredMethod("getBasebandFilter", double.class);
            baseband.setAccessible(true);
            print(out, "baseband_25k", (float[])baseband.invoke(new P25P1DecoderLSM(), 25000.0));
            print(out, "rrc_25k", FilterFactory.getRootRaisedCosine(25000.0 / 4800.0, 16, 0.2f));
        }
        else if(args.length >= 2 && args[0].equals("decode"))
        {
            File outDir = new File(args[1]);
            outDir.mkdirs();
            for(int a = 2; a < args.length; a++)
            {
                decode(out, new File(args[a]), outDir);
            }
        }
        else
        {
            System.err.println("usage: LsmWavHarness taps | decode OUT_DIR WAV...");
            System.exit(2);
        }
        System.exit(0);
    }

    private static void print(PrintStream out, String name, float[] taps)
    {
        StringBuilder sb = new StringBuilder(name).append('|').append(taps.length).append('|');
        for(int k = 0; k < taps.length; k++)
        {
            sb.append(k == 0 ? "" : ",").append(Float.toString(taps[k]));
        }
        out.println(sb);
    }

    private static void decode(PrintStream out, File file, File outDir) throws Exception
    {
        String name = file.getName();
        ByteArrayOutputStream bits = new ByteArrayOutputStream();
        P25P1DecoderLSM decoder = new P25P1DecoderLSM();
        decoder.start();
        decoder.setMessageListener(m -> out.println(name + "|" + m.getTimestamp() + "|" + m.isValid() + "|"
            + m.getClass().getSimpleName() + "|" + m));
        decoder.setBufferListener((ByteBuffer buffer) -> {
            byte[] chunk = new byte[buffer.remaining()];
            buffer.get(chunk);
            bits.write(chunk, 0, chunk.length);
        });
        decoder.setSampleRate(50000.0);
        try(DataInputStream in = new DataInputStream(new BufferedInputStream(new FileInputStream(file))))
        {
            in.skipNBytes(44);
            int block = 1250;
            byte[] raw = new byte[block * 4];
            long timestamp = 1;
            while(true)
            {
                int n = in.readNBytes(raw, 0, raw.length);
                //P25P1DemodulatorLSM fails on a block shorter than the one before it, so the
                //file's last partial block is left out.
                if(n < raw.length)
                {
                    break;
                }
                int count = n / 4;
                float[] i = new float[count];
                float[] q = new float[count];
                for(int k = 0; k < count; k++)
                {
                    int b = k * 4;
                    short si = (short)((raw[b] & 0xFF) | (raw[b + 1] << 8));
                    short sq = (short)((raw[b + 2] & 0xFF) | (raw[b + 3] << 8));
                    i[k] = si / 32768.0f;
                    q[k] = sq / 32768.0f;
                }
                decoder.receive(new ComplexSamples(i, q, timestamp));
                timestamp += count * 1000L / 50000L;
            }
        }
        decoder.stop();
        String stem = name.endsWith(".wav") ? name.substring(0, name.length() - 4) : name;
        try(FileOutputStream f = new FileOutputStream(new File(outDir, stem + ".bits")))
        {
            bits.writeTo(f);
        }
    }
}
