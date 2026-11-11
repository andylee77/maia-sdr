// Reads playlists with SDRTrunk's own reader (as PlaylistManager sets it up) and writes each as
// lines of text, the same lines the scanner's `sdrtrunk::Playlist::dump` gives:
//
//   channel|system|site|name|enabled=..|decoder|mod=..|ium=..|freqs=..|lcn=..|list=..
//   alias|list|name|group|color=..|icon=..|ids
//
// Run by tools/sdrtrunk_playlist_reference.py: PlaylistDump <in> <out> [<in> <out> ...]

import com.fasterxml.jackson.annotation.JsonSubTypes;
import com.fasterxml.jackson.databind.DeserializationFeature;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.dataformat.xml.JacksonXmlModule;
import com.fasterxml.jackson.dataformat.xml.XmlMapper;
import io.github.dsheirer.alias.Alias;
import io.github.dsheirer.alias.id.AliasID;
import io.github.dsheirer.alias.id.priority.Priority;
import io.github.dsheirer.alias.id.radio.Radio;
import io.github.dsheirer.alias.id.radio.RadioRange;
import io.github.dsheirer.alias.id.record.Record;
import io.github.dsheirer.alias.id.talkgroup.Talkgroup;
import io.github.dsheirer.alias.id.talkgroup.TalkgroupRange;
import io.github.dsheirer.controller.channel.Channel;
import io.github.dsheirer.module.decode.config.DecodeConfiguration;
import io.github.dsheirer.module.decode.dmr.DecodeConfigDMR;
import io.github.dsheirer.module.decode.dmr.channel.TimeslotFrequency;
import io.github.dsheirer.module.decode.p25.phase1.DecodeConfigP25;
import io.github.dsheirer.module.decode.p25.phase1.DecodeConfigP25Phase1;
import io.github.dsheirer.playlist.PlaylistV2;
import io.github.dsheirer.source.config.SourceConfigTuner;
import io.github.dsheirer.source.config.SourceConfigTunerMultipleFrequency;
import io.github.dsheirer.source.config.SourceConfiguration;

import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;

public class PlaylistDump
{
    // A class's name in a playlist's `type`, from the annotations SDRTrunk reads it by.
    private static final Map<Class<?>, String> TYPES = new HashMap<>();

    private static void types(Class<?> base)
    {
        JsonSubTypes subtypes = base.getAnnotation(JsonSubTypes.class);

        if(subtypes != null)
        {
            for(JsonSubTypes.Type type : subtypes.value())
            {
                TYPES.put(type.value(), type.name());
                if(type.value() != base)
                {
                    types(type.value());
                }
            }
        }
    }

    private static String text(Object value)
    {
        return value == null ? "" : value.toString();
    }

    private static String type(Object value)
    {
        return value == null ? "" : TYPES.getOrDefault(value.getClass(), value.getClass().getSimpleName());
    }

    public static void main(String[] args) throws Exception
    {
        types(AliasID.class);
        types(DecodeConfiguration.class);

        JacksonXmlModule xmlModule = new JacksonXmlModule();
        xmlModule.setDefaultUseWrapper(false);
        ObjectMapper mapper = new XmlMapper(xmlModule).configure(DeserializationFeature.FAIL_ON_UNKNOWN_PROPERTIES, false);

        for(int i = 0; i + 1 < args.length; i += 2)
        {
            PlaylistV2 playlist;

            try(InputStream in = Files.newInputStream(Path.of(args[i])))
            {
                playlist = mapper.readValue(in, PlaylistV2.class);
            }

            StringBuilder out = new StringBuilder();

            for(Channel channel : playlist.getChannels())
            {
                DecodeConfiguration decode = channel.getDecodeConfiguration();
                SourceConfiguration source = channel.getSourceConfiguration();
                List<String> frequencies = new ArrayList<>();

                if(source instanceof SourceConfigTuner tuner)
                {
                    frequencies.add(Long.toString(tuner.getFrequency()));
                }
                else if(source instanceof SourceConfigTunerMultipleFrequency multiple)
                {
                    for(Long frequency : multiple.getFrequencies())
                    {
                        frequencies.add(Long.toString(frequency));
                    }
                }

                Map<Integer, Long> numbers = new TreeMap<>();

                if(decode instanceof DecodeConfigDMR dmr)
                {
                    for(TimeslotFrequency timeslot : dmr.getTimeslotMap())
                    {
                        numbers.put(timeslot.getNumber(), timeslot.getDownlinkFrequency());
                    }
                }

                List<String> lcns = new ArrayList<>();
                numbers.forEach((number, frequency) -> lcns.add(number + ":" + frequency));

                out.append("channel|").append(text(channel.getSystem())).append('|').append(text(channel.getSite())).append('|')
                    .append(text(channel.getName())).append("|enabled=").append(channel.getAutoStart()).append('|').append(type(decode))
                    .append("|mod=").append(decode instanceof DecodeConfigP25Phase1 p1 ? p1.getModulation().name() : "-")
                    .append("|ium=").append(decode instanceof DecodeConfigP25 p25 ? Boolean.toString(p25.getIgnoreUnmonitoredCalls()) : "-")
                    .append("|freqs=").append(String.join(",", frequencies)).append("|lcn=").append(String.join(",", lcns))
                    .append("|list=").append(text(channel.getAliasListName())).append('\n');
            }

            for(Alias alias : playlist.getAliases())
            {
                List<String> ids = new ArrayList<>();

                for(AliasID id : alias.getAliasIdentifiers())
                {
                    if(id instanceof Talkgroup talkgroup)
                    {
                        ids.add("talkgroup:" + talkgroup.getProtocol().name() + ":" + talkgroup.getValue());
                    }
                    else if(id instanceof TalkgroupRange range)
                    {
                        ids.add("talkgroupRange:" + range.getProtocol().name() + ":" + range.getMinTalkgroup() + "-" + range.getMaxTalkgroup());
                    }
                    else if(id instanceof Radio radio)
                    {
                        ids.add("radio:" + radio.getProtocol().name() + ":" + radio.getValue());
                    }
                    else if(id instanceof RadioRange range)
                    {
                        ids.add("radioRange:" + range.getProtocol().name() + ":" + range.getMinRadio() + "-" + range.getMaxRadio());
                    }
                    else if(id instanceof Priority priority)
                    {
                        ids.add("priority:" + priority.getPriority());
                    }
                    else if(id instanceof Record)
                    {
                        ids.add("record");
                    }
                    else
                    {
                        ids.add(type(id));
                    }
                }

                out.append("alias|").append(text(alias.getAliasListName())).append('|').append(text(alias.getName())).append('|')
                    .append(text(alias.getGroup())).append("|color=").append(alias.getColor()).append("|icon=").append(text(alias.getIconName()))
                    .append('|').append(String.join(" ", ids)).append('\n');
            }

            Files.writeString(Path.of(args[i + 1]), out.toString(), StandardCharsets.UTF_8);
            System.out.println(args[i] + ": " + playlist.getChannels().size() + " channels, " + playlist.getAliases().size() + " aliases");
        }

        System.exit(0);
    }
}
