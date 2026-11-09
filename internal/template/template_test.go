package template

import (
	"strings"
	"testing"

	"github.com/eraser-privacy/eraser/internal/broker"
	"github.com/eraser-privacy/eraser/internal/config"
)

func TestRenderAllTemplates(t *testing.T) {
	e, err := NewEngine()
	if err != nil {
		t.Fatal(err)
	}
	p := config.Profile{FirstName: "Jürg", LastName: "Muster", Email: "j@example.ch", City: "Zürich"}
	b := broker.Broker{Name: "Acme Data AG", Region: "ch"}
	want := map[string]string{
		"gdpr":    "Article 21(3)",
		"ccpa":    "§ 1798.120",
		"fadp":    "Art. 30 para. 2 lit. b FADP",
		"generic": "Swiss Federal Act on Data Protection",
	}
	for name, needle := range want {
		m, err := e.Render(name, p, b)
		if err != nil {
			t.Fatalf("%s: %v", name, err)
		}
		if !strings.Contains(m.Body, needle) || !strings.Contains(m.Body, "Jürg Muster") || !strings.Contains(m.Body, "Acme Data AG") {
			t.Errorf("%s: body missing expected text %q", name, needle)
		}
		if strings.ContainsAny(m.Subject, "\r\n") || m.Subject == "" {
			t.Errorf("%s: bad subject %q", name, m.Subject)
		}
		for _, r := range m.Subject {
			if r > 127 {
				t.Errorf("%s: subject must be ASCII (sent unencoded): %q", name, m.Subject)
			}
		}
	}
}

func TestAutoPicksLawByBrokerRegion(t *testing.T) {
	e, _ := NewEngine()
	for region, tmpl := range map[string]string{"ch": "fadp", "eu": "gdpr", "us": "generic", "global": "generic", "CH": "fadp"} {
		m, err := e.Render("auto", config.Profile{FirstName: "A", LastName: "B", Email: "a@b.ch"}, broker.Broker{Name: "X", Region: region})
		if err != nil || m.Template != tmpl {
			t.Errorf("region %s: got %q (%v), want %s", region, m.Template, err, tmpl)
		}
	}
	if ForBroker("ccpa", broker.Broker{Region: "ch"}) != "ccpa" {
		t.Error("explicit template must not be overridden")
	}
}
