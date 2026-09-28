package httpapi

import (
	"net/http"

	"github.com/acme/shop/internal/orders"
)

type Handler struct {
	svc *orders.Service
}

func (h *Handler) Routes(mux *http.ServeMux) {
	mux.HandleFunc("POST /orders/{id}/cancel", h.cancel)
	mux.HandleFunc("POST /orders/{id}/pay", h.pay)
}

func (h *Handler) cancel(w http.ResponseWriter, r *http.Request) {
	user := r.Header.Get("X-User")
	if err := h.svc.Cancel(r.Context(), user, r.PathValue("id")); err != nil {
		http.Error(w, err.Error(), http.StatusConflict)
		return
	}
	w.WriteHeader(http.StatusNoContent)
}

func (h *Handler) pay(w http.ResponseWriter, r *http.Request) {
	if err := h.svc.Pay(r.Context(), r.PathValue("id")); err != nil {
		http.Error(w, err.Error(), http.StatusConflict)
		return
	}
	w.WriteHeader(http.StatusNoContent)
}
