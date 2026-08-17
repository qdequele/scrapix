# Building blocks for the branded dark HTML emails — ported from the Rust
# template helpers in the retired bins/scrapix-api/src/email.rs.
module MailerHelper
  CONSOLE_URL = ENV.fetch("CONSOLE_PUBLIC_URL", "https://scrapix.meilisearch.com")

  def console_url(path = "")
    "#{CONSOLE_URL}#{path}"
  end

  def em_heading(text)
    tag.h1(text.html_safe, style: "margin: 0 0 16px; font-size: 22px; font-weight: 700; color: #fafafa; line-height: 1.3;")
  end

  def em_paragraph(text)
    tag.p(text.html_safe, style: "margin: 0 0 16px; font-size: 15px; line-height: 1.6; color: #a1a1aa;")
  end

  def em_button(url, label)
    <<~HTML.html_safe
      <table role="presentation" cellpadding="0" cellspacing="0" style="margin: 24px 0;">
        <tr>
          <td style="background-color: #fff; border-radius: 10px;">
            <a href="#{url}" style="display: inline-block; padding: 12px 28px; font-size: 14px; font-weight: 600; color: #09090b; text-decoration: none; border-radius: 10px;">#{label}</a>
          </td>
        </tr>
      </table>
    HTML
  end

  def em_table(&block)
    content = capture(&block)
    <<~HTML.html_safe
      <table role="presentation" width="100%" cellpadding="0" cellspacing="0" style="margin: 20px 0; border-collapse: collapse;">
        #{content}
      </table>
    HTML
  end

  def em_kv_row(label, value, last: false, mono: false)
    border = last ? "" : "border-bottom: 1px solid rgba(255,255,255,0.06);"
    value_style =
      if mono
        "padding: 12px 0; text-align: right; font-size: 13px; font-family: 'SF Mono', SFMono-Regular, Consolas, 'Liberation Mono', Menlo, monospace; color: #a1a1aa;"
      else
        "padding: 12px 0; text-align: right; font-size: 14px; font-weight: 600; color: #e4e4e7;"
      end
    <<~HTML.html_safe
      <tr style="#{border}">
        <td style="padding: 12px 0; font-size: 14px; color: #71717a;">#{ERB::Util.html_escape(label)}</td>
        <td style="#{value_style}">#{ERB::Util.html_escape(value)}</td>
      </tr>
    HTML
  end

  def em_muted(text)
    tag.p(text.html_safe, style: "margin: 16px 0 0; font-size: 12px; line-height: 1.5; color: #52525b;")
  end

  def em_link(url, text)
    tag.a(text, href: url, style: "color: #818cf8; text-decoration: underline;")
  end

  def em_strong(text)
    tag.strong(text, style: "color: #e4e4e7;")
  end

  def em_alert(text, style)
    bg, border, color =
      case style
      when :error then [ "#2a1215", "rgba(248,113,113,0.2)", "#fca5a5" ]
      when :warning then [ "#2a2012", "rgba(251,191,36,0.2)", "#fcd34d" ]
      end
    <<~HTML.html_safe
      <div style="background: #{bg}; border: 1px solid #{border}; border-radius: 10px; padding: 16px; margin: 16px 0;">
        <p style="margin: 0; font-size: 14px; line-height: 1.5; color: #{color};">#{text}</p>
      </div>
    HTML
  end

  def em_code(text)
    tag.code(text, style: "background: #27272a; padding: 2px 6px; border-radius: 4px; font-size: 13px; color: #818cf8;")
  end

  def format_duration(secs)
    secs = secs.to_i
    if secs < 60 then "#{secs}s"
    elsif secs < 3600 then "#{secs / 60}m #{secs % 60}s"
    else "#{secs / 3600}h #{(secs % 3600) / 60}m"
    end
  end

  def format_dollars(amount_cents)
    format("$%.2f", amount_cents.to_i / 100.0)
  end
end
