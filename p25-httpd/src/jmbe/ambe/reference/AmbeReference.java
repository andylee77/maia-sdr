import java.io.IOException;
import java.io.PrintWriter;
import java.lang.reflect.Field;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.file.Files;
import java.nio.file.Paths;
import java.util.Arrays;
import java.util.Random;

import jmbe.codec.MBEModelParameters;
import jmbe.codec.ambe.AMBEFrame;
import jmbe.iface.IAudioCodec;
import jmbe.iface.IAudioCodecLibrary;
import jmbe.iface.IAudioWithMetadata;

/**
 * Decodes AMBE 3600x2450 frames with jmbe (the reference jar) for the Rust
 * port's comparison test.
 *
 * Usage: AmbeReference frames.bin pcm.f32 [audio.wav] [frames.txt]
 *
 * frames.bin holds 9-byte frames; pcm.f32 gets 160 little-endian floats per
 * frame, exactly as IAudioCodec.getAudioWithMetadata() returns them. The
 * optional WAV is 8 kHz 16-bit, scaled as SDRTrunk does. frames.txt lists
 * each frame's FEC, b-vector, metadata and resulting model parameters.
 *
 * jmbe seeds its comfort-noise Random from the clock; this harness replaces
 * it with new Random(SEED) advanced past the 257 draws of jmbe's
 * WhiteNoiseGenerator constructor, which the Rust port reproduces.
 */
public class AmbeReference
{
    static final long SEED = 20260930L;

    public static void main(String[] args) throws Exception
    {
        byte[] data = Files.readAllBytes(Paths.get(args[0]));
        int count = data.length / 9;

        IAudioCodecLibrary library = (IAudioCodecLibrary)Class.forName("jmbe.JMBEAudioLibrary")
            .getDeclaredConstructor().newInstance();
        IAudioCodec codec = library.getAudioConverter("AMBE 3600 x 2450");
        Object synthesizer = field(codec.getClass(), "mSynthesizer").get(codec);
        Object whiteNoise = field(synthesizer.getClass().getSuperclass(), "mWhiteNoiseGenerator").get(synthesizer);
        Random random = new Random(SEED);
        for(int x = 0; x < 257; x++)
        {
            random.nextFloat();
        }
        field(whiteNoise.getClass(), "mRandom").set(whiteNoise, random);
        Field previousFrame = field(synthesizer.getClass(), "mPreviousFrame");
        Field bVector = field(AMBEFrame.class, "mB");

        ByteBuffer pcm = ByteBuffer.allocate(count * 160 * 4).order(ByteOrder.LITTLE_ENDIAN);
        ByteBuffer wav = ByteBuffer.allocate(count * 160 * 2).order(ByteOrder.LITTLE_ENDIAN);
        PrintWriter text = args.length > 3 ? new PrintWriter(args[3]) : null;

        for(int f = 0; f < count; f++)
        {
            byte[] frame = Arrays.copyOfRange(data, f * 9, f * 9 + 9);
            IAudioWithMetadata audio = codec.getAudioWithMetadata(frame);
            for(float sample : audio.getAudio())
            {
                pcm.putFloat(sample);
                wav.putShort((short)(sample * Short.MAX_VALUE));
            }

            if(text != null)
            {
                AMBEFrame ambe = new AMBEFrame(frame);
                MBEModelParameters p = (MBEModelParameters)previousFrame.get(synthesizer);
                text.println(f + " " + ambe + " B:" + Arrays.toString((int[])bVector.get(ambe)) +
                    " META:" + audio.getMetadata());
                text.println("  PREV: L:" + p.getL() + " W0:" + p.getFundamentalFrequency() +
                    " RATE:" + p.getErrorRate() + " REPEAT:" + p.getRepeatCount() +
                    " ENERGY:" + p.getLocalEnergy() + " TM:" + p.getAmplitudeThreshold() +
                    " V:" + Arrays.toString(p.getVoicingDecisions()) +
                    " M:" + Arrays.toString(p.getEnhancedSpectralAmplitudes()));
            }
        }

        Files.write(Paths.get(args[1]), pcm.array());
        if(args.length > 2 && !args[2].isEmpty())
        {
            writeWav(args[2], wav.array());
        }
        if(text != null)
        {
            text.close();
        }
        System.out.println(count + " frames");
    }

    static Field field(Class<?> type, String name) throws NoSuchFieldException
    {
        Field field = type.getDeclaredField(name);
        field.setAccessible(true);
        return field;
    }

    static void writeWav(String path, byte[] samples) throws IOException
    {
        ByteBuffer header = ByteBuffer.allocate(44).order(ByteOrder.LITTLE_ENDIAN);
        header.put("RIFF".getBytes()).putInt(36 + samples.length).put("WAVEfmt ".getBytes());
        header.putInt(16).putShort((short)1).putShort((short)1).putInt(8000).putInt(16000);
        header.putShort((short)2).putShort((short)16).put("data".getBytes()).putInt(samples.length);
        byte[] out = new byte[44 + samples.length];
        System.arraycopy(header.array(), 0, out, 0, 44);
        System.arraycopy(samples, 0, out, 44, samples.length);
        Files.write(Paths.get(path), out);
    }
}
