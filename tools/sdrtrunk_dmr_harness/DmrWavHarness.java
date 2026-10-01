import io.github.dsheirer.message.IMessage;
import io.github.dsheirer.module.decode.dmr.DMRDecoder;
import io.github.dsheirer.module.decode.dmr.DecodeConfigDMR;
import io.github.dsheirer.sample.complex.ComplexSamples;
import java.io.DataInputStream;
import java.io.FileInputStream;
import java.io.BufferedInputStream;
import java.io.PrintStream;

/**
 * Runs SDRTrunk's DMRDecoder over 50 kSPS stereo int16 WAV captures (p25-httpd
 * /api/control_iq_dump) and prints one line per message:
 * file|timestamp_ms|timeslot|valid|class|toString
 */
public class DmrWavHarness
{
    public static void main(String[] args) throws Exception
    {
        PrintStream out = new PrintStream(System.out, true, "UTF-8");
        for(String file : args)
        {
            String name = new java.io.File(file).getName();
            DMRDecoder decoder = new DMRDecoder(new DecodeConfigDMR(), false);
            decoder.start();
            decoder.setMessageListener(m -> out.println(name + "|" + m.getTimestamp() + "|" + m.getTimeslot() + "|"
                + m.isValid() + "|" + m.getClass().getSimpleName() + "|" + m));
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
                    if(n < 4)
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
        }
        System.exit(0);
    }
}
