package template

import (
	"bytes"
	"embed"
	"fmt"
	"strings"
	"text/template"
	"time"

	"github.com/eraser-privacy/eraser/internal/broker"
	"github.com/eraser-privacy/eraser/internal/config"
)

//go:embed templates/*.tmpl
var embeddedTemplates embed.FS

// EmailData contains all data available to email templates
type EmailData struct {
	// User profile
	FirstName   string
	LastName    string
	FullName    string
	Email       string
	Address     string
	City        string
	State       string
	ZipCode     string
	Country     string
	Phone       string
	DateOfBirth string

	// Broker info
	BrokerName    string
	BrokerEmail   string
	BrokerWebsite string
	BrokerOptOut  string

	// Metadata
	Date     string
	Year     int
	Month    string
	Template string
}

// Email represents a rendered email ready to send
type Email struct {
	Subject  string
	Body     string
	Template string // template actually used (resolved when "auto")
}

// ForBroker resolves "auto" to the law that binds the broker: Swiss FADP for
// Swiss brokers, GDPR for EU/UK brokers, the multi-law generic letter otherwise.
func ForBroker(templateName string, b broker.Broker) string {
	if templateName != "auto" {
		return templateName
	}
	switch strings.ToLower(b.Region) {
	case "ch":
		return "fadp"
	case "eu":
		return "gdpr"
	default:
		return "generic"
	}
}

// Engine handles email template rendering
type Engine struct {
	templates map[string]*template.Template
}

// NewEngine creates a new template engine
func NewEngine() (*Engine, error) {
	e := &Engine{
		templates: make(map[string]*template.Template),
	}

	templateNames := []string{"gdpr", "ccpa", "fadp", "generic"}
	for _, name := range templateNames {
		content, err := embeddedTemplates.ReadFile("templates/" + name + ".tmpl")
		if err != nil {
			return nil, fmt.Errorf("failed to read embedded template %s: %w", name, err)
		}

		tmpl, err := template.New(name).Parse(string(content))
		if err != nil {
			return nil, fmt.Errorf("failed to parse template %s: %w", name, err)
		}

		e.templates[name] = tmpl
	}

	return e, nil
}

// Render generates an email from a template
func (e *Engine) Render(templateName string, profile config.Profile, b broker.Broker) (*Email, error) {
	templateName = ForBroker(templateName, b)
	tmpl, ok := e.templates[templateName]
	if !ok {
		return nil, fmt.Errorf("unknown template: %s", templateName)
	}

	now := time.Now()
	data := EmailData{
		FirstName:     profile.FirstName,
		LastName:      profile.LastName,
		FullName:      profile.FullName(),
		Email:         profile.Email,
		Address:       profile.Address,
		City:          profile.City,
		State:         profile.State,
		ZipCode:       profile.ZipCode,
		Country:       profile.Country,
		Phone:         profile.Phone,
		DateOfBirth:   profile.DateOfBirth,
		BrokerName:    b.Name,
		BrokerEmail:   b.Email,
		BrokerWebsite: b.Website,
		BrokerOptOut:  b.OptOutURL,
		Date:          now.Format("January 2, 2006"),
		Year:          now.Year(),
		Month:         now.Format("January"),
		Template:      templateName,
	}

	var buf bytes.Buffer
	if err := tmpl.Execute(&buf, data); err != nil {
		return nil, fmt.Errorf("failed to render template: %w", err)
	}

	subject := e.getSubject(templateName, b.Name)

	return &Email{
		Subject:  subject,
		Body:     buf.String(),
		Template: templateName,
	}, nil
}

func (e *Engine) getSubject(templateName, brokerName string) string {
	switch templateName {
	case "gdpr":
		return "Erasure request and objection under Articles 17 and 21 GDPR"
	case "ccpa":
		return "CCPA Request to Delete and to Opt Out of Sale/Sharing"
	case "fadp":
		return "Deletion request and objection under Articles 30 and 32 FADP (DSG)"
	default:
		return "Personal Data Deletion and Opt-Out Request"
	}
}

// AvailableTemplates returns the list of available template names
func (e *Engine) AvailableTemplates() []string {
	templates := make([]string, 0, len(e.templates))
	for name := range e.templates {
		templates = append(templates, name)
	}
	return templates
}
